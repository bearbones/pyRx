use crate::graph::ProgramIndex;
use crate::model::*;
use crate::snapshot::write_file_atomically;
use ruff_python_ast::{Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMorphChange {
    pub file: String,
    pub edit_count: usize,
    pub descriptions: Vec<String>,
    pub unified_diff: String,
    #[serde(skip)]
    pub new_content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphReport {
    pub dry_run: bool,
    pub files_changed: usize,
    pub total_edits: usize,
    pub changes: Vec<FileMorphChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MorphRulesFile {
    pub rules: Vec<MorphRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MorphRule {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Target callee qualified name or short name (e.g. `"subprocess.run"`, `"pkg.api.legacy_fetch"`).
    pub match_callee: String,
    /// Optional condition: caller's immediate execution context (`"async"`, `"sync"`, `"module_init"`).
    #[serde(default)]
    pub when_caller_execution_context: Option<String>,
    /// Optional condition: true if the caller is an `async def` or transitively reachable from an `async def`.
    #[serde(default)]
    pub when_reachable_from_async: Option<bool>,
    /// Action kind: `"wrap_asyncio_to_thread"`, `"replace_callee"`, `"wrap_arguments_in_constructor"`, `"template"`.
    pub action: String,
    #[serde(default)]
    pub replacement_callee: Option<String>,
    #[serde(default)]
    pub wrapper_class: Option<String>,
    #[serde(default)]
    pub unpack_dict_arg: Option<bool>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub ensure_import: Option<String>,
}

/// Runs declarative cross-module codemod rules (`pyrex morph --rules-json <file>`) conditioned on `megascope` facts.
pub fn run_declarative_rules(
    index: &ProgramIndex,
    project_root: &Path,
    rules: &[MorphRule],
    dry_run: bool,
) -> Result<MorphReport, String> {
    crate::cache::verify_source_fingerprints(index, project_root)?;
    // Precompute the set of entities reachable from any `async def` coroutine
    let reachable_from_async = compute_entities_reachable_from_async(index);

    let mut edits_by_file: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
    let mut imports_to_add_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut source_cache: BTreeMap<String, String> = BTreeMap::new();

    let mut load_file = |rel_file: &str| -> Result<String, String> {
        if let Some(s) = source_cache.get(rel_file) {
            return Ok(s.clone());
        }
        let abs = project_root.join(rel_file);
        let content = fs::read_to_string(&abs)
            .map_err(|e| format!("Failed to read '{}': {e}", abs.display()))?;
        let expected = index
            .meta
            .modules
            .iter()
            .find(|module| module.file == rel_file)
            .ok_or_else(|| format!("Source '{rel_file}' is not in the index"))?;
        if xxhash_rust::xxh3::xxh3_64(content.as_bytes()) != expected.fingerprint {
            return Err(format!(
                "Stale index: '{rel_file}' changed while planning; no files were written"
            ));
        }
        source_cache.insert(rel_file.to_string(), content.clone());
        Ok(content)
    };

    for edge in &index.calls {
        for rule in rules {
            if !callee_matches_rule(edge, &rule.match_callee) {
                continue;
            }
            if let Some(req_ctx) = &rule.when_caller_execution_context {
                let actual_ctx = match edge.execution_context {
                    ExecutionContext::Async => "async",
                    ExecutionContext::Sync => "sync",
                    ExecutionContext::StaticMethod => "staticmethod",
                    ExecutionContext::ClassMethod => "classmethod",
                    ExecutionContext::ModuleInit => "module_init",
                    ExecutionContext::ThreadSpawn => "thread_spawn",
                    ExecutionContext::PackagedTask => "packaged_task",
                    ExecutionContext::Invoke => "invoke",
                };
                if actual_ctx != req_ctx {
                    continue;
                }
            }
            if let Some(req_async_reach) = rule.when_reachable_from_async {
                let is_reach = reachable_from_async.contains(&edge.caller);
                if is_reach != req_async_reach {
                    continue;
                }
            }

            let src = load_file(&edge.file)?;
            let call_start = edge.byte_start as usize;
            let call_end = (edge.byte_end as usize).min(src.len());
            if call_start >= call_end {
                continue;
            }
            let call_slice = &src[call_start..call_end];

            match rule.action.as_str() {
                "wrap_asyncio_to_thread" => {
                    if edge.execution_context != ExecutionContext::Async {
                        return Err(format!(
                            "Rule '{}' would create an unawaited coroutine in synchronous function '{}'; offload at the async boundary instead",
                            rule.id, edge.caller
                        ));
                    }
                    let Some((callee_s, args_s)) = split_call_callee_and_args(call_slice) else {
                        continue;
                    };
                    let wrapped = if args_s.trim().is_empty() {
                        format!("asyncio.to_thread({callee_s})")
                    } else {
                        format!("asyncio.to_thread({callee_s}, {args_s})")
                    };
                    let replacement =
                        if edge.execution_context == ExecutionContext::Async && !edge.is_awaited {
                            format!("await {wrapped}")
                        } else {
                            wrapped
                        };
                    edits_by_file
                        .entry(edge.file.clone())
                        .or_default()
                        .push(TextEdit {
                            byte_start: call_start,
                            byte_end: call_end,
                            replacement,
                            description: format!(
                                "[{}] Wrap blocking call '{}' in asyncio.to_thread",
                                rule.id, edge.callee
                            ),
                        });
                    imports_to_add_by_file
                        .entry(edge.file.clone())
                        .or_default()
                        .insert(
                            rule.ensure_import
                                .clone()
                                .unwrap_or_else(|| "import asyncio".to_string()),
                        );
                }
                "replace_callee" => {
                    let Some(new_callee) = &rule.replacement_callee else {
                        return Err(format!(
                            "Rule '{}' with action 'replace_callee' requires 'replacement_callee'",
                            rule.id
                        ));
                    };
                    if let Some((c_start, c_end, _)) =
                        find_call_callee_span(&src, call_start, call_end)
                    {
                        edits_by_file
                            .entry(edge.file.clone())
                            .or_default()
                            .push(TextEdit {
                                byte_start: c_start,
                                byte_end: c_end,
                                replacement: new_callee.clone(),
                                description: format!(
                                    "[{}] Replace callee '{}' -> '{}'",
                                    rule.id, edge.callee_raw, new_callee
                                ),
                            });
                        if let Some(imp) = &rule.ensure_import {
                            imports_to_add_by_file
                                .entry(edge.file.clone())
                                .or_default()
                                .insert(imp.clone());
                        }
                    }
                }
                "wrap_arguments_in_constructor" => {
                    let new_callee = rule
                        .replacement_callee
                        .as_deref()
                        .unwrap_or(&edge.callee_raw);
                    let Some(wrapper_cls) = &rule.wrapper_class else {
                        return Err(format!(
                            "Rule '{}' with action 'wrap_arguments_in_constructor' requires 'wrapper_class'",
                            rule.id
                        ));
                    };
                    let Some((_callee_s, args_s)) = split_call_callee_and_args(call_slice) else {
                        continue;
                    };
                    let unpack = rule.unpack_dict_arg.unwrap_or(false);
                    let trimmed_args = args_s.trim();
                    let inner_args = if unpack && !trimmed_args.starts_with("**") {
                        format!("**{trimmed_args}")
                    } else {
                        trimmed_args.to_string()
                    };
                    let replacement = format!("{new_callee}({wrapper_cls}({inner_args}))");
                    edits_by_file
                        .entry(edge.file.clone())
                        .or_default()
                        .push(TextEdit {
                            byte_start: call_start,
                            byte_end: call_end,
                            replacement,
                            description: format!(
                                "[{}] Migrate '{}' to '{}({}(...))'",
                                rule.id, edge.callee_raw, new_callee, wrapper_cls
                            ),
                        });
                    if let Some(imp) = &rule.ensure_import {
                        imports_to_add_by_file
                            .entry(edge.file.clone())
                            .or_default()
                            .insert(imp.clone());
                    }
                }
                "template" => {
                    let Some(tmpl) = &rule.template else {
                        return Err(format!(
                            "Rule '{}' with action 'template' requires 'template'",
                            rule.id
                        ));
                    };
                    let (callee_s, args_s) =
                        split_call_callee_and_args(call_slice).unwrap_or((call_slice, ""));
                    let replacement = tmpl
                        .replace("{call}", call_slice)
                        .replace("{callee}", callee_s)
                        .replace("{args}", args_s);
                    edits_by_file
                        .entry(edge.file.clone())
                        .or_default()
                        .push(TextEdit {
                            byte_start: call_start,
                            byte_end: call_end,
                            replacement,
                            description: format!(
                                "[{}] Apply template rewrite to '{}'",
                                rule.id, edge.callee
                            ),
                        });
                    if let Some(imp) = &rule.ensure_import {
                        imports_to_add_by_file
                            .entry(edge.file.clone())
                            .or_default()
                            .insert(imp.clone());
                    }
                }
                other => {
                    return Err(format!(
                        "Unknown morph rule action '{other}' in rule '{}'",
                        rule.id
                    ));
                }
            }
        }
    }

    for (file, stmts) in imports_to_add_by_file {
        let src = load_file(&file)?;
        let mut needed = Vec::new();
        for st in stmts {
            if !has_top_level_import(&src, &st) {
                needed.push(st);
            }
        }
        if !needed.is_empty() {
            let insert_offset = find_import_insertion_offset(&src);
            let block = needed.join("\n") + "\n";
            edits_by_file.entry(file).or_default().push(TextEdit {
                byte_start: insert_offset,
                byte_end: insert_offset,
                replacement: block,
                description: format!("Add required import(s): {}", needed.join("; ")),
            });
        }
    }

    apply_edits_and_build_report(&source_cache, edits_by_file, project_root, dry_run)
}

fn callee_matches_rule(edge: &CallEdge, pattern: &str) -> bool {
    edge.callee == pattern
        || edge.callee_raw == pattern
        || edge.callee.ends_with(&format!(".{pattern}"))
}

fn compute_entities_reachable_from_async(index: &ProgramIndex) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();
    for ent in &index.entities {
        if ent.kind.is_async() || ent.execution_context == ExecutionContext::Async {
            if reachable.insert(ent.qualified_name.clone()) {
                queue.push_back(ent.qualified_name.clone());
            }
        }
    }
    while let Some(cur) = queue.pop_front() {
        for edge in index.effective_callees(&cur) {
            if edge.kind == CallEdgeKind::HigherOrderCallback {
                continue;
            }
            if index.entity_by_qname.contains_key(&edge.callee)
                && reachable.insert(edge.callee.clone())
            {
                queue.push_back(edge.callee);
            }
        }
    }
    reachable
}

fn find_call_callee_span(
    source: &str,
    call_start: usize,
    call_end: usize,
) -> Option<(usize, usize, &str)> {
    let end = call_end.min(source.len());
    if call_start >= end {
        return None;
    }
    let slice = &source[call_start..end];
    let paren_rel = slice.find('(')?;
    let callee_end = call_start + paren_rel;
    let callee_text = source[call_start..callee_end].trim();
    Some((call_start, callee_end, callee_text))
}

fn split_call_callee_and_args(call_slice: &str) -> Option<(&str, &str)> {
    let open_paren = call_slice.find('(')?;
    let close_paren = call_slice.rfind(')')?;
    if open_paren >= close_paren {
        return None;
    }
    let callee = call_slice[..open_paren].trim();
    let args = &call_slice[open_paren + 1..close_paren];
    Some((callee, args))
}

fn has_top_level_import(source: &str, statement: &str) -> bool {
    parse_module(source).is_ok_and(|parsed| {
        parsed.syntax().body.iter().any(|stmt| {
            matches!(stmt, Stmt::Import(_) | Stmt::ImportFrom(_))
                && source[stmt.range().start().to_usize()..stmt.range().end().to_usize()].trim()
                    == statement
        })
    })
}

/// Finds the byte offset at the top of a Python module right after any shebang, encoding comment,
/// module docstring, or `from __future__ import ...` statements.
fn find_import_insertion_offset(source: &str) -> usize {
    if let Ok(parsed) = parse_module(source) {
        let module = parsed.into_syntax();
        let mut insert_after = 0usize;
        for line in source.split_inclusive('\n').take(2) {
            if line.starts_with("#!")
                || (line.trim_start().starts_with('#')
                    && (line.contains("coding:") || line.contains("coding=")))
            {
                insert_after += line.len();
            } else {
                break;
            }
        }
        for (idx, stmt) in module.body.iter().enumerate() {
            // Skip leading module docstring
            if idx == 0 {
                if let Stmt::Expr(e) = stmt {
                    if matches!(&*e.value, Expr::StringLiteral(_)) {
                        insert_after = advance_past_line_end(source, stmt.range().end().to_usize());
                        continue;
                    }
                }
            }
            // Skip `from __future__ import ...`
            if let Stmt::ImportFrom(imp) = stmt {
                if imp.module.as_ref().map(|m| m.id.as_str()) == Some("__future__") {
                    insert_after = advance_past_line_end(source, stmt.range().end().to_usize());
                    continue;
                }
            }
            break;
        }
        return insert_after;
    }
    0
}

fn advance_past_line_end(source: &str, mut offset: usize) -> usize {
    let bytes = source.as_bytes();
    while offset < bytes.len() && bytes[offset] != b'\n' {
        offset += 1;
    }
    if offset < bytes.len() && bytes[offset] == b'\n' {
        offset += 1;
    }
    offset
}

fn apply_edits_and_build_report(
    source_cache: &BTreeMap<String, String>,
    edits_by_file: BTreeMap<String, Vec<TextEdit>>,
    project_root: &Path,
    dry_run: bool,
) -> Result<MorphReport, String> {
    let mut changes = Vec::new();
    let mut total_edits = 0usize;

    for (file, mut edits) in edits_by_file {
        if edits.is_empty() {
            continue;
        }
        let Some(orig) = source_cache.get(&file) else {
            continue;
        };

        // Sort edits by (byte_start, byte_end) and deduplicate identical edits
        edits.sort_by(|a, b| {
            a.byte_start
                .cmp(&b.byte_start)
                .then_with(|| a.byte_end.cmp(&b.byte_end))
                .then_with(|| a.replacement.cmp(&b.replacement))
        });
        edits.dedup_by(|a, b| {
            a.byte_start == b.byte_start
                && a.byte_end == b.byte_end
                && a.replacement == b.replacement
        });

        // Filter out any edit strictly contained inside a larger deletion edit (e.g. an import or call inside a deleted span)
        let mut non_overlapping: Vec<TextEdit> = Vec::with_capacity(edits.len());
        for edit in edits {
            if let Some(prev) = non_overlapping.last() {
                if edit.byte_start < prev.byte_end {
                    // If `prev` is a zero-width insertion at `edit.byte_start`, allow it!
                    if prev.byte_start == prev.byte_end && prev.byte_end == edit.byte_start {
                        non_overlapping.push(edit);
                        continue;
                    }
                    // If `edit` is inside a larger deletion span `prev`, skip `edit`
                    if edit.byte_end <= prev.byte_end && prev.replacement.is_empty() {
                        continue;
                    }
                    return Err(format!(
                        "Conflicting overlapping CST edits in '{}' at byte offsets {}..{} and {}..{}",
                        file, prev.byte_start, prev.byte_end, edit.byte_start, edit.byte_end
                    ));
                }
            }
            non_overlapping.push(edit);
        }

        let mut new_content = orig.clone();
        let mut descriptions = Vec::new();
        for edit in non_overlapping.iter().rev() {
            new_content.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        for edit in &non_overlapping {
            descriptions.push(edit.description.clone());
        }

        if &new_content == orig {
            continue;
        }

        parse_module(&new_content).map_err(|error| {
            format!("Rule produced invalid Python in '{file}': {error}; no files were written")
        })?;
        let diff = TextDiff::from_lines(orig.as_str(), new_content.as_str())
            .unified_diff()
            .header(&format!("a/{file}"), &format!("b/{file}"))
            .to_string();

        let edit_count = non_overlapping.len();
        total_edits += edit_count;
        changes.push(FileMorphChange {
            file,
            edit_count,
            descriptions,
            unified_diff: diff,
            new_content,
        });
    }

    if !dry_run {
        // Recheck the complete plan immediately before applying any file.
        for (file, original) in source_cache {
            let current = fs::read_to_string(project_root.join(file))
                .map_err(|e| format!("Cannot recheck '{file}': {e}"))?;
            if current != *original {
                return Err(format!(
                    "Source '{file}' changed while planning; no files were written"
                ));
            }
        }
        for change in &changes {
            let abs = project_root.join(&change.file);
            write_file_atomically(&abs, change.new_content.as_bytes())?;
        }
    }

    Ok(MorphReport {
        dry_run,
        files_changed: changes.len(),
        total_edits,
        changes,
    })
}
