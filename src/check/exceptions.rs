use crate::check::evidence::{call_path, compact_path};
use crate::check::{CheckSeverity, Finding};
use crate::graph::ProgramIndex;
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub fn check_uncaught_exception_escape(
    index: &ProgramIndex,
    severity: CheckSeverity,
) -> Vec<Finding> {
    check_uncaught_exception_escape_with_options(index, severity, false)
}

pub fn check_uncaught_exception_escape_with_options(
    index: &ProgramIndex,
    severity: CheckSeverity,
    check_main: bool,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for ent in &index.entities {
        if crate::workspace::is_test_path(&ent.file, &ent.module) {
            continue;
        }
        let uncaught_raises: Vec<&RaiseSite> =
            ent.raises.iter().filter(|r| !r.is_caught_locally).collect();
        if uncaught_raises.is_empty() {
            continue;
        }

        for raise_site in uncaught_raises {
            let exc_type = &raise_site.exception_type;
            if exc_type == "NotImplementedError" || exc_type.ends_with(".NotImplementedError") {
                continue;
            }

            // Walk reverse call graph from `ent` to see if `exc_type` escapes any non-test entry point
            let mut queue: VecDeque<(String, Vec<CallEdge>)> = VecDeque::new();
            let mut visited = BTreeSet::new();
            visited.insert(ent.qualified_name.clone());
            queue.push_back((ent.qualified_name.clone(), Vec::new()));

            while let Some((cur, rev_chain)) = queue.pop_front() {
                if let Some(&cur_idx) = index.entity_by_qname.get(&cur) {
                    let cur_ent = &index.entities[cur_idx];
                    if is_boundary_entry_point(cur_ent, check_main) {
                        let mut forward_path = rev_chain.clone();
                        forward_path.reverse();
                        findings.push(Finding::new(
                            "uncaught-exception-escape",
                            "uncaught_exception_escape",
                            severity,
                            &cur_ent.file,
                            cur_ent.line,
                            cur_ent.column,
                            &cur_ent.qualified_name,
                            vec![
                                cur_ent.qualified_name.clone(),
                                ent.qualified_name.clone(),
                                exc_type.clone(),
                            ],
                            format!(
                                "Uncaught '{}' raised in '{}' ({}:{}) escapes entry point '{}' via path: {}",
                                exc_type,
                                ent.qualified_name,
                                ent.file,
                                raise_site.line,
                                cur_ent.qualified_name,
                                if forward_path.is_empty() { cur.clone() } else { compact_path(&call_path(&forward_path)) }
                            ),
                            None,
                        ).with_evidence(&forward_path));
                        // Keep searching: the same exception may escape other boundaries too.
                    }
                }

                for edge in index.effective_callers(&cur) {
                    if edge.kind == CallEdgeKind::HigherOrderCallback {
                        continue;
                    }
                    if let Some(ctx) = index.contexts.get(edge.context_id as usize) {
                        if call_site_catches(ctx, exc_type) {
                            continue;
                        }
                    }
                    if visited.insert(edge.caller.clone()) {
                        let mut next_chain = rev_chain.clone();
                        next_chain.push(edge.clone());
                        queue.push_back((edge.caller, next_chain));
                    }
                }
            }
        }
    }

    findings
}

fn is_boundary_entry_point(ent: &EntityNode, check_main: bool) -> bool {
    if !ent.is_entry_point || crate::workspace::is_test_path(&ent.file, &ent.module) {
        return false;
    }
    if ent.kind == EntityKind::Module {
        return check_main
            && matches!(
                ent.entry_point_kind.as_deref(),
                Some("main_guard") | Some("pyproject_script") | Some("configured")
            );
    }
    match ent.entry_point_kind.as_deref() {
        Some("pyproject_script")
        | Some("framework:homeassistant")
        | Some("cli_decorator")
        | Some("route_decorator")
        | Some("configured") => true,
        Some("main_guard") => check_main,
        _ => false,
    }
}

fn call_site_catches(ctx: &CallSiteContext, exc_type: &str) -> bool {
    let short_exc = exc_type.rsplit('.').next().unwrap_or(exc_type);
    for ts in &ctx.try_scopes {
        for h in &ts.handlers {
            if h.rethrows {
                continue;
            }
            if h.is_bare_except {
                return true;
            }
            for ct in &h.caught_types {
                let short_ct = ct.rsplit('.').next().unwrap_or(ct);
                if short_ct == "Exception"
                    || short_ct == "BaseException"
                    || ct == exc_type
                    || short_ct == short_exc
                    || (short_ct == "OSError"
                        && matches!(
                            short_exc,
                            "IOError" | "FileNotFoundError" | "PermissionError"
                        ))
                    || (short_ct == "ValueError" && short_exc == "JSONDecodeError")
                {
                    return true;
                }
            }
        }
    }
    false
}

pub fn check_broad_catch_swallow(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();

    for ent in &index.entities {
        if crate::workspace::is_test_path(&ent.file, &ent.module) {
            continue;
        }
        let callers = index.effective_callers(&ent.qualified_name);
        let is_async = ent.kind.is_async() || ent.execution_context == ExecutionContext::Async;

        for catch in &ent.broad_catches {
            if !catch.swallows || catch.rethrows {
                continue;
            }
            let caught_label = if catch.is_bare_except {
                "bare 'except:'".to_string()
            } else {
                format!("'except {}:'", catch.caught_types.join(", "))
            };

            let async_cancel_hazard = is_async
                && (catch.is_bare_except
                    || catch
                        .caught_types
                        .iter()
                        .any(|t| t == "BaseException" || t.ends_with(".BaseException")))
                && !catch.handles_cancelled_error;

            let msg = if async_cancel_hazard {
                format!(
                    "Broad {} in async '{}' swallows exceptions without re-raising 'asyncio.CancelledError'",
                    caught_label, ent.qualified_name
                )
            } else if !callers.is_empty() {
                format!(
                    "Broad {} in '{}' swallows exceptions without re-raising (called by {} caller(s): {})",
                    caught_label,
                    ent.qualified_name,
                    callers.len(),
                    callers
                        .iter()
                        .take(3)
                        .map(|c| c.caller.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                format!(
                    "Broad {} in '{}' silently swallows exceptions without re-raising",
                    caught_label, ent.qualified_name
                )
            };

            findings.push(Finding::new(
                "broad-catch-swallow",
                if async_cancel_hazard {
                    "async_cancelled_swallow"
                } else {
                    "broad_catch_swallow"
                },
                severity,
                &ent.file,
                catch.line,
                catch.column,
                &ent.qualified_name,
                vec![ent.qualified_name.clone(), caught_label],
                msg,
                None,
            ));
        }
    }

    findings
}

pub fn check_exception_contract_divergence(
    index: &ProgramIndex,
    severity: CheckSeverity,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    let transitive_raises = transitive_uncaught_raises(index);

    for (base_method, overrides) in &index.method_overrides {
        let base_raises = transitive_raises
            .get(base_method)
            .cloned()
            .unwrap_or_default();

        let base_contract_label = if let Some(&base_idx) = index.entity_by_qname.get(base_method) {
            let base_ent = &index.entities[base_idx];
            format!("'{}' ({}:{})", base_method, base_ent.file, base_ent.line)
        } else if let Some((base_cls, _)) = base_method.rsplit_once('.') {
            if let Some(&cls_idx) = index.entity_by_qname.get(base_cls) {
                let cls_ent = &index.entities[cls_idx];
                format!("'{}' ({}:{})", base_method, cls_ent.file, cls_ent.line)
            } else {
                format!("'{}'", base_method)
            }
        } else {
            format!("'{}'", base_method)
        };

        // Collect all raises across base and siblings
        let mut raises_by_impl: Vec<(&str, &BTreeSet<String>)> = Vec::new();
        for ov in overrides {
            if let Some(rset) = transitive_raises.get(ov) {
                raises_by_impl.push((ov.as_str(), rset));
            }
        }

        for (ov_qname, ov_raises) in &raises_by_impl {
            if ov_raises.is_empty() {
                continue;
            }
            let Some(&ov_idx) = index.entity_by_qname.get(*ov_qname) else {
                continue;
            };
            let ov_ent = &index.entities[ov_idx];

            // An exception in `ov_raises` diverges if:
            // - `base_raises` is non-empty and doesn't include it, OR
            // - `base_raises` is empty (e.g. Protocol/ABC stub) and at least one sibling implementation exists that does not raise it (or base method is a Protocol/ABC method and override raises undeclared runtime exceptions while siblings don't).
            let mut divergent: Vec<String> = Vec::new();
            for exc in *ov_raises {
                if exc == "NotImplementedError" || exc.ends_with(".NotImplementedError") {
                    continue;
                }
                let in_base = base_raises.contains(exc);
                if !base_raises.is_empty() && !in_base {
                    divergent.push(exc.clone());
                } else if base_raises.is_empty() && raises_by_impl.len() >= 2 {
                    let all_siblings_raise = raises_by_impl
                        .iter()
                        .all(|(_, s_raises)| s_raises.contains(exc));
                    if !all_siblings_raise {
                        divergent.push(exc.clone());
                    }
                }
            }

            if !divergent.is_empty() {
                divergent.sort();
                findings.push(Finding::new(
                    "exception-contract-divergence",
                    "override_exception_divergence",
                    severity,
                    &ov_ent.file,
                    ov_ent.line,
                    ov_ent.column,
                    &ov_ent.qualified_name,
                    vec![
                        base_method.clone(),
                        ov_ent.qualified_name.clone(),
                        divergent.join(","),
                    ],
                    format!(
                        "Method override '{}' raises [{}] not raised by base contract {} or sibling implementations",
                        ov_ent.qualified_name,
                        divergent.join(", "),
                        base_contract_label
                    ),
                    None,
                ));
            }
        }
    }

    findings
}

fn transitive_uncaught_raises(index: &ProgramIndex) -> BTreeMap<String, BTreeSet<String>> {
    // Propagate each exception to a fixed point; each (symbol, exception) is visited once.
    let mut summaries: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for ent in &index.entities {
        let set = summaries.entry(ent.qualified_name.clone()).or_default();
        for raise in &ent.raises {
            if !raise.is_caught_locally
                && raise.exception_type.rsplit('.').next() != Some("NotImplementedError")
                && set.insert(raise.exception_type.clone())
            {
                queue.push_back((ent.qualified_name.clone(), raise.exception_type.clone()));
            }
        }
    }
    while let Some((callee, exception)) = queue.pop_front() {
        for edge in index.effective_callers(&callee) {
            if edge.kind == CallEdgeKind::HigherOrderCallback
                || index
                    .contexts
                    .get(edge.context_id as usize)
                    .is_some_and(|ctx| call_site_catches(ctx, &exception))
            {
                continue;
            }
            if summaries
                .entry(edge.caller.clone())
                .or_default()
                .insert(exception.clone())
            {
                queue.push_back((edge.caller, exception.clone()));
            }
        }
    }
    summaries
}
