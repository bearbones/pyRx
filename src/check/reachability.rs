use crate::check::{CheckSeverity, Finding};
use crate::graph::ProgramIndex;
use crate::model::*;
use crate::query::compute_reachable_entities;
use std::collections::{BTreeSet, VecDeque};

/// Collects public interface entry points from `__all__`, package `__init__.py` re-exports,
/// public methods of exported classes, and configured `[entry_points]` in `.pyrx.toml`.
pub fn collect_public_interface_entry_points(
    index: &ProgramIndex,
    cfg_entry_points: &[String],
) -> Vec<String> {
    let mut extra_eps: BTreeSet<String> = BTreeSet::new();
    for ep in cfg_entry_points {
        extra_eps.insert(ep.clone());
        if let Some((m, s)) = ep.split_once(':') {
            extra_eps.insert(format!("{m}.{s}"));
        }
    }

    // 1. Symbols imported/re-exported in package `__init__.py`
    let mut init_reexports: BTreeSet<(String, String)> = BTreeSet::new();
    for imp in &index.imports {
        let is_init = imp.file == "__init__.py" || imp.file.ends_with("/__init__.py");
        if !is_init || imp.is_type_checking || imp.is_lazy {
            continue;
        }
        if let Some(sym) = &imp.imported_symbol {
            let exposed = imp.alias.as_deref().unwrap_or(sym);
            if (sym == "*" || !exposed.starts_with('_')) && !imp.target_module.is_empty() {
                init_reexports.insert((imp.target_module.clone(), sym.clone()));
                if sym != "*" {
                    extra_eps.insert(format!("{}.{sym}", imp.target_module));
                }
            }
        }
    }

    // Follow multi-hop re-exports across modules (up to 4 hops)
    for _ in 0..4 {
        let current: Vec<(String, String)> = init_reexports.iter().cloned().collect();
        let mut added = false;
        for (tmod, tsym) in current {
            for imp in &index.imports {
                if imp.importer_module != tmod || imp.is_type_checking || imp.is_lazy {
                    continue;
                }
                if let Some(sym) = &imp.imported_symbol {
                    let local_name = imp.alias.as_deref().unwrap_or(sym);
                    if (tsym == "*" && !local_name.starts_with('_')) || local_name == tsym {
                        if !imp.target_module.is_empty()
                            && init_reexports.insert((imp.target_module.clone(), sym.clone()))
                        {
                            extra_eps.insert(format!("{}.{sym}", imp.target_module));
                            added = true;
                        }
                    }
                }
            }
        }
        if !added {
            break;
        }
    }

    // 2. Match entities against re-exports and collect exported classes
    let mut exported_classes: BTreeSet<String> = BTreeSet::new();
    for ent in &index.entities {
        if ent.kind == EntityKind::Module {
            continue;
        }
        let is_init_def = (ent.file == "__init__.py" || ent.file.ends_with("/__init__.py"))
            && ent.enclosing_class.is_none()
            && !ent.name.starts_with('_');
        let is_reexported = ent.enclosing_class.is_none()
            && (init_reexports.contains(&(ent.module.clone(), ent.name.clone()))
                || (!ent.name.starts_with('_')
                    && init_reexports.contains(&(ent.module.clone(), "*".to_string()))));
        let is_cfg_ep = extra_eps.contains(&ent.qualified_name)
            || extra_eps.contains(&format!("{}:{}", ent.module, ent.name));

        if is_init_def || is_reexported || is_cfg_ep || ent.is_exported {
            extra_eps.insert(ent.qualified_name.clone());
            if ent.kind == EntityKind::Class {
                exported_classes.insert(ent.qualified_name.clone());
            }
        } else if ent.kind == EntityKind::Class && ent.is_entry_point {
            exported_classes.insert(ent.qualified_name.clone());
        }
    }

    // 3. Public methods on exported classes are part of the public interface
    if !exported_classes.is_empty() {
        for ent in &index.entities {
            if let Some(cls) = &ent.enclosing_class {
                if exported_classes.contains(cls) && !ent.name.starts_with('_') {
                    extra_eps.insert(ent.qualified_name.clone());
                }
            }
        }
    }

    extra_eps.into_iter().collect()
}

pub fn check_dead_code(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    check_dead_code_with_entry_points(index, severity, &[])
}

pub fn check_dead_code_with_entry_points(
    index: &ProgramIndex,
    severity: CheckSeverity,
    cfg_entry_points: &[String],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let extra_eps = collect_public_interface_entry_points(index, cfg_entry_points);

    // Only emit dead-code if the index has at least one entry point to root reachability from
    let has_entry_point = index.entities.iter().any(|e| e.is_entry_point) || !extra_eps.is_empty();
    if !has_entry_point {
        return findings;
    }

    let reachable = compute_reachable_entities(index, &extra_eps);

    for ent in &index.entities {
        if matches!(
            ent.kind,
            EntityKind::Module
                | EntityKind::Lambda
                | EntityKind::Comprehension
                | EntityKind::Generator
        ) {
            continue;
        }
        if crate::workspace::is_test_path(&ent.file, &ent.module)
            || ent.file.starts_with("examples/")
            || ent.file.starts_with("docs/")
        {
            continue;
        }
        if reachable.contains(&ent.qualified_name) {
            continue;
        }
        // If this is a method of a dead class, avoid noisy double-reporting if the enclosing class itself is already reported dead,
        // UNLESS the method participates in a dead mutual-call subgraph.
        let callers = index.effective_callers(&ent.qualified_name);
        if let Some(cls) = &ent.enclosing_class {
            if !reachable.contains(cls) && callers.is_empty() {
                continue;
            }
        }

        let dead_callers: Vec<String> = callers.into_iter().map(|c| c.caller).collect();
        let msg = if dead_callers.is_empty() {
            format!(
                "{:?} '{}' is unreachable from any entry point in the repository (if part of the public interface, export via '__all__', re-export in package '__init__.py', or add to '[entry_points]' in '.pyrx.toml'; symbols only called by unit tests are reported under 'test-only-production-code')",
                ent.kind, ent.qualified_name
            )
        } else {
            format!(
                "{:?} '{}' belongs to an unreachable dead subgraph (only called by unreachable: {}; if part of the public interface, export via '__all__', re-export in package '__init__.py', or add to '[entry_points]' in '.pyrx.toml')",
                ent.kind,
                ent.qualified_name,
                dead_callers.join(", ")
            )
        };

        findings.push(Finding::new(
            "dead-code",
            if dead_callers.is_empty() {
                "unreachable_symbol"
            } else {
                "dead_subgraph"
            },
            severity,
            &ent.file,
            ent.line,
            ent.column,
            &ent.qualified_name,
            vec![ent.qualified_name.clone()],
            msg,
            None,
        ));
    }

    findings
}

pub fn check_unused_parameter(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    check_unused_parameter_with_options(index, severity, false, true)
}

pub fn check_unused_parameter_with_options(
    index: &ProgramIndex,
    severity: CheckSeverity,
    check_dunders: bool,
    check_constant_args: bool,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Collect all methods that are overrides of a base/protocol method
    let mut overridden_methods = BTreeSet::new();
    for (base_m, ovs) in &index.method_overrides {
        overridden_methods.insert(base_m.as_str());
        for ov in ovs {
            overridden_methods.insert(ov.as_str());
        }
    }

    // Collect functions registered as signal handlers via `signal.signal(sig, handler)`
    let mut signal_handlers = BTreeSet::new();
    for edge in &index.calls {
        if edge.callee == "signal.signal" || edge.callee.ends_with(".signal.signal") {
            for caller_edge in &index.calls {
                if caller_edge.caller == edge.caller
                    && caller_edge.line == edge.line
                    && caller_edge.kind == CallEdgeKind::HigherOrderCallback
                {
                    signal_handlers.insert(caller_edge.callee.as_str());
                }
            }
        }
    }

    for ent in &index.entities {
        if !matches!(
            ent.kind,
            EntityKind::Function
                | EntityKind::AsyncFunction
                | EntityKind::Method
                | EntityKind::AsyncMethod
        ) {
            continue;
        }
        if crate::workspace::is_test_path(&ent.file, &ent.module)
            || ent.file.starts_with("examples/")
            || ent.file.starts_with("docs/")
        {
            continue;
        }
        if !check_dunders && crate::indexer::is_magic_dunder_method(&ent.name) {
            continue;
        }
        if ent.is_abstractmethod
            || ent.statement_count == 0
            || ent.skeleton_hash == 0
            || overridden_methods.contains(ent.qualified_name.as_str())
            || signal_handlers.contains(ent.qualified_name.as_str())
            || ent.decorators.iter().any(|d| {
                d == "abstractmethod"
                    || d.ends_with(".abstractmethod")
                    || d == "overload"
                    || d.ends_with(".overload")
            })
        {
            continue;
        }
        if let Some(cls) = &ent.enclosing_class {
            if let Some(&c_idx) = index.entity_by_qname.get(cls) {
                let cls_ent = &index.entities[c_idx];
                if cls_ent.is_protocol
                    || cls_ent.is_abc
                    || cls_ent.bases.iter().any(|b| {
                        b == "Protocol"
                            || b.ends_with(".Protocol")
                            || b == "ABC"
                            || b.ends_with(".ABC")
                    })
                {
                    continue;
                }
            }
        }

        let callers = index.effective_callers(&ent.qualified_name);
        let non_self_params: Vec<(usize, &ParamInfo)> = ent
            .parameters
            .iter()
            .filter(|p| p.name != "self" && p.name != "cls")
            .enumerate()
            .collect();

        // Standard 2-parameter signal handler signature `(signum, frame)` / `(sig, frame)`
        if non_self_params.len() == 2 {
            let p0 = non_self_params[0].1.name.as_str();
            let p1 = non_self_params[1].1.name.as_str();
            if matches!(p0, "signum" | "sig" | "signal_num" | "signo")
                && matches!(p1, "frame" | "stack_frame")
            {
                continue;
            }
        }

        for (pos_idx, param) in non_self_params {
            if param.name.starts_with('_')
                || matches!(param.kind, ParamKind::VarArg | ParamKind::KwArg)
            {
                continue;
            }

            if !param.used_in_body {
                findings.push(Finding::new(
                    "unused-parameter",
                    "unread_parameter",
                    severity,
                    &ent.file,
                    param.line,
                    param.column,
                    &ent.qualified_name,
                    vec![ent.qualified_name.clone(), param.name.clone()],
                    format!(
                        "Parameter '{}' of '{}' is never read in the function body or overrides (note: if kept for backwards compatibility with callers or interface contracts, prefix with an underscore '_{}' — Python's convention analogous to unnamed parameters 'Type /*{}*/' or '[[maybe_unused]]' in C++)",
                        param.name, ent.qualified_name, param.name, param.name
                    ),
                    None,
                ));
                continue;
            }

            // Check if every call site across the repo (>= 2 call sites) passes the exact same constant literal
            if check_constant_args && callers.len() >= 2 {
                let pos_key = pos_idx.to_string();
                let mut constants_passed = Vec::new();
                let mut all_constant = true;
                for edge in &callers {
                    let found = edge
                        .arg_constants
                        .iter()
                        .find(|(k, _)| k == &param.name || k == &pos_key)
                        .and_then(|(_, val)| val.clone());
                    match found {
                        Some(c) => constants_passed.push(c),
                        None => {
                            all_constant = false;
                            break;
                        }
                    }
                }
                if all_constant && !constants_passed.is_empty() {
                    let first = &constants_passed[0];
                    if constants_passed.iter().all(|c| c == first) {
                        findings.push(Finding::new(
                            "unused-parameter",
                            "constant_parameter_across_callers",
                            severity,
                            &ent.file,
                            param.line,
                            param.column,
                            &ent.qualified_name,
                            vec![ent.qualified_name.clone(), param.name.clone()],
                            format!(
                                "Parameter '{}' of '{}' receives the same constant {} at all {} call sites across the repository (note: if kept for backwards compatibility with callers, prefix with '_{}' or assign a default value)",
                                param.name,
                                ent.qualified_name,
                                first,
                                callers.len(),
                                param.name
                            ),
                            None,
                        ));
                    }
                }
            }
        }
    }

    findings
}

pub fn check_unused_parameter_cross_tu(
    index: &ProgramIndex,
    severity: CheckSeverity,
) -> Vec<Finding> {
    check_unused_parameter(index, severity)
}

pub fn check_test_only_production_code(
    index: &ProgramIndex,
    severity: CheckSeverity,
) -> Vec<Finding> {
    check_test_only_production_code_with_entry_points(index, severity, &[])
}

pub fn check_test_only_production_code_with_entry_points(
    index: &ProgramIndex,
    severity: CheckSeverity,
    cfg_entry_points: &[String],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Only run if the index contains at least one test module and at least one production module
    let has_test_mod = index
        .meta
        .modules
        .iter()
        .any(|m| crate::workspace::is_test_path(&m.file, &m.module_name));
    if !has_test_mod {
        return findings;
    }

    let public_eps: BTreeSet<String> =
        collect_public_interface_entry_points(index, cfg_entry_points)
            .into_iter()
            .collect();

    for ent in &index.entities {
        if matches!(
            ent.kind,
            EntityKind::Module
                | EntityKind::Lambda
                | EntityKind::Comprehension
                | EntityKind::Generator
        ) {
            continue;
        }
        if crate::workspace::is_test_path(&ent.file, &ent.module) {
            continue;
        }
        if ent.is_entry_point || ent.is_exported || public_eps.contains(&ent.qualified_name) {
            continue;
        }

        let direct_callers = index.effective_callers(&ent.qualified_name);
        if direct_callers.is_empty() {
            continue;
        }

        // Walk reverse call graph to inspect all callers
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(ent.qualified_name.clone());
        queue.push_back(ent.qualified_name.clone());

        let mut has_prod_caller = false;
        let mut test_callers = BTreeSet::new();

        while let Some(cur) = queue.pop_front() {
            for edge in index.effective_callers(&cur) {
                let caller_is_test = crate::workspace::is_test_path(&edge.file, &edge.module)
                    || edge.caller.contains(".test_")
                    || edge.caller.contains(".Test");
                if caller_is_test {
                    test_callers.insert(edge.caller.clone());
                } else {
                    has_prod_caller = true;
                    break;
                }
                if visited.insert(edge.caller.clone()) {
                    queue.push_back(edge.caller);
                }
            }
            if has_prod_caller {
                break;
            }
        }

        if !has_prod_caller && !test_callers.is_empty() {
            let caller_list: Vec<String> = test_callers.into_iter().take(3).collect();
            findings.push(Finding::new(
                "test-only-production-code",
                "test_only_production_symbol",
                severity,
                &ent.file,
                ent.line,
                ent.column,
                &ent.qualified_name,
                vec![ent.qualified_name.clone()],
                format!(
                    "Production {:?} '{}' in '{}' is only called from test code ({}); if part of the public interface, export via '__all__', re-export in '__init__.py', or add to '[entry_points]' in '.pyrx.toml'",
                    ent.kind,
                    ent.qualified_name,
                    ent.file,
                    caller_list.join(", ")
                ),
                None,
            ));
        }
    }

    findings
}
