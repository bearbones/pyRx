use crate::check::{CheckSeverity, Finding};
use crate::graph::ProgramIndex;
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub fn check_import_side_effects(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();

    // 1. Check module-top-level environment mutations (`os.environ[...] = ...`, `os.putenv`)
    for acc in &index.env_accesses {
        if acc.is_mutation && acc.enclosing_entity.ends_with(".<module>") {
            findings.push(Finding::new(
                "import-side-effects",
                "module_env_mutation",
                severity,
                &acc.file,
                acc.line,
                acc.column,
                &acc.enclosing_entity,
                vec![acc.enclosing_entity.clone(), acc.var_name.clone()],
                format!(
                    "Module '{}' mutates environment variable '{}' at import time",
                    acc.module, acc.var_name
                ),
                None,
            ));
        }
    }

    // 2. Check unguarded `<module>` calls that directly or transitively perform I/O, subprocesses, or thread spawning
    for mod_ent in &index.entities {
        if mod_ent.kind != EntityKind::Module {
            continue;
        }
        let Some(top_edge_indices) = index.callees_by_caller.get(&mod_ent.qualified_name) else {
            continue;
        };

        for &e_idx in top_edge_indices {
            let top_edge = &index.calls[e_idx];
            if top_edge.kind == CallEdgeKind::Decorator {
                continue;
            }
            if let Some(ctx) = index.contexts.get(top_edge.context_id as usize) {
                let in_main_or_tc = ctx.guards.iter().any(|g| {
                    g.in_true_branch
                        && matches!(g.kind, GuardKind::MainGuard | GuardKind::TypeChecking)
                });
                if in_main_or_tc {
                    continue;
                }
            }

            // Check if `top_edge.callee` directly or transitively executes an impure import-time side effect
            if let Some((effect_kind, effect_callee, chain)) =
                find_transitive_side_effect(top_edge, index, 5)
            {
                let path_str = if chain.len() <= 1 {
                    effect_callee.clone()
                } else {
                    chain.join(" -> ")
                };
                findings.push(Finding::new(
                    "import-side-effects",
                    effect_kind,
                    severity,
                    &top_edge.file,
                    top_edge.line,
                    top_edge.column,
                    &mod_ent.qualified_name,
                    vec![mod_ent.qualified_name.clone(), effect_callee.clone()],
                    format!(
                        "Module '{}' performs '{}' ('{}') at import time outside 'if __name__ == \"__main__\":' (path: {})",
                        mod_ent.module, effect_kind, effect_callee, path_str
                    ),
                    None,
                ));
            }
        }
    }

    findings
}

fn find_transitive_side_effect(
    top_edge: &CallEdge,
    index: &ProgramIndex,
    max_depth: usize,
) -> Option<(&'static str, String, Vec<String>)> {
    if let Some(kind) = classify_side_effect_call(&top_edge.callee, &top_edge.arg_constants) {
        return Some((
            kind,
            top_edge.callee.clone(),
            vec![top_edge.caller.clone(), top_edge.callee.clone()],
        ));
    }

    if !index.entity_by_qname.contains_key(&top_edge.callee) {
        return None;
    }

    let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
    let mut visited = BTreeSet::new();
    visited.insert(top_edge.caller.clone());
    visited.insert(top_edge.callee.clone());
    queue.push_back((
        top_edge.callee.clone(),
        vec![top_edge.caller.clone(), top_edge.callee.clone()],
    ));

    while let Some((cur, path)) = queue.pop_front() {
        // Check if `cur` mutates os.environ
        if index
            .env_accesses
            .iter()
            .any(|a| a.is_mutation && a.enclosing_entity == cur)
        {
            return Some(("env_mutation", cur, path));
        }

        if path.len() > max_depth + 1 {
            continue;
        }
        for edge in index.effective_callees(&cur) {
            let mut next_path = path.clone();
            next_path.push(edge.callee.clone());
            if let Some(kind) = classify_side_effect_call(&edge.callee, &edge.arg_constants) {
                return Some((kind, edge.callee, next_path));
            }
            if index.entity_by_qname.contains_key(&edge.callee)
                && visited.insert(edge.callee.clone())
            {
                queue.push_back((edge.callee, next_path));
            }
        }
    }
    None
}

fn classify_side_effect_call(
    callee: &str,
    arg_constants: &[(String, Option<String>)],
) -> Option<&'static str> {
    if matches!(
        callee,
        "subprocess.run"
            | "subprocess.Popen"
            | "subprocess.call"
            | "subprocess.check_call"
            | "subprocess.check_output"
            | "os.system"
    ) {
        return Some("subprocess_execution");
    }
    if callee.starts_with("requests.")
        || matches!(
            callee,
            "urllib.request.urlopen"
                | "urlopen"
                | "httpx.get"
                | "httpx.post"
                | "httpx.put"
                | "httpx.delete"
                | "httpx.request"
                | "socket.create_connection"
        )
    {
        return Some("network_io");
    }
    if callee.ends_with(".write_text")
        || callee.ends_with(".write_bytes")
        || matches!(
            callee,
            "os.remove" | "os.unlink" | "os.makedirs" | "os.mkdir" | "shutil.rmtree"
        )
    {
        return Some("file_write");
    }
    if callee == "open" {
        let is_write = arg_constants.iter().any(|(k, v)| {
            (k == "1" || k == "mode")
                && v.as_deref()
                    .is_some_and(|m| m.contains('w') || m.contains('a') || m.contains('+'))
        });
        if is_write {
            return Some("file_write");
        }
    }
    if matches!(
        callee,
        "threading.Thread" | "multiprocessing.Process" | "Thread.start" | "Process.start"
    ) {
        return Some("thread_or_process_spawn");
    }
    None
}

pub fn check_import_cycle(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();
    let known_modules: BTreeSet<&str> = index
        .meta
        .modules
        .iter()
        .map(|m| m.module_name.as_str())
        .collect();

    // Build runtime top-level import adjacency (`!is_type_checking && !is_lazy`)
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut first_edge_site: BTreeMap<(String, String), &ImportEdge> = BTreeMap::new();

    for imp in &index.imports {
        if imp.is_type_checking || imp.is_lazy {
            continue;
        }
        if imp.importer_module == imp.target_module {
            continue;
        }
        if known_modules.contains(imp.importer_module.as_str())
            && known_modules.contains(imp.target_module.as_str())
        {
            adj.entry(imp.importer_module.clone())
                .or_default()
                .insert(imp.target_module.clone());
            first_edge_site
                .entry((imp.importer_module.clone(), imp.target_module.clone()))
                .or_insert(imp);
        }
    }

    // Tarjan's SCC algorithm
    let sccs = tarjan_sccs(&adj);
    for mut scc in sccs {
        if scc.len() < 2 {
            continue;
        }
        scc.sort();
        // Reconstruct a deterministic cycle within `scc` starting from `scc[0]`
        let start = &scc[0];
        let scc_set: BTreeSet<String> = scc.iter().cloned().collect();
        let cycle_chain = find_cycle_in_scc(start, &adj, &scc_set);
        let next_mod = cycle_chain.get(1).unwrap_or(&scc[1]);

        if let Some(&edge) = first_edge_site.get(&(start.clone(), next_mod.clone())) {
            findings.push(Finding::new(
                "import-cycle",
                "runtime_import_cycle",
                severity,
                &edge.file,
                edge.line,
                edge.column,
                format!("{start}.<module>"),
                scc.clone(),
                format!(
                    "Runtime top-level import cycle across {} modules: {} (guard type-only imports with 'if TYPE_CHECKING:' or move inside functions)",
                    scc.len(),
                    cycle_chain.join(" -> ")
                ),
                None,
            ));
        }
    }

    findings
}

fn tarjan_sccs(adj: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    struct TarjanState<'a> {
        adj: &'a BTreeMap<String, BTreeSet<String>>,
        index: usize,
        indices: BTreeMap<String, usize>,
        lowlink: BTreeMap<String, usize>,
        stack: Vec<String>,
        on_stack: BTreeSet<String>,
        sccs: Vec<Vec<String>>,
    }

    fn strongconnect(v: &str, st: &mut TarjanState<'_>) {
        st.indices.insert(v.to_string(), st.index);
        st.lowlink.insert(v.to_string(), st.index);
        st.index += 1;
        st.stack.push(v.to_string());
        st.on_stack.insert(v.to_string());

        if let Some(neighbors) = st.adj.get(v) {
            for w in neighbors {
                if !st.indices.contains_key(w) {
                    strongconnect(w, st);
                    let lw = st.lowlink[w];
                    let lv = st.lowlink.get_mut(v).unwrap();
                    *lv = (*lv).min(lw);
                } else if st.on_stack.contains(w) {
                    let iw = st.indices[w];
                    let lv = st.lowlink.get_mut(v).unwrap();
                    *lv = (*lv).min(iw);
                }
            }
        }

        if st.lowlink[v] == st.indices[v] {
            let mut scc = Vec::new();
            while let Some(w) = st.stack.pop() {
                st.on_stack.remove(&w);
                scc.push(w.clone());
                if w == v {
                    break;
                }
            }
            st.sccs.push(scc);
        }
    }

    let mut st = TarjanState {
        adj,
        index: 0,
        indices: BTreeMap::new(),
        lowlink: BTreeMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        sccs: Vec::new(),
    };

    for node in adj.keys() {
        if !st.indices.contains_key(node) {
            strongconnect(node, &mut st);
        }
    }
    st.sccs
}

fn find_cycle_in_scc(
    start: &str,
    adj: &BTreeMap<String, BTreeSet<String>>,
    scc_set: &BTreeSet<String>,
) -> Vec<String> {
    let mut queue: VecDeque<(String, Vec<String>)> = VecDeque::new();
    let mut visited = BTreeSet::new();
    visited.insert(start.to_string());
    queue.push_back((start.to_string(), vec![start.to_string()]));

    while let Some((cur, path)) = queue.pop_front() {
        if let Some(neighbors) = adj.get(&cur) {
            for nxt in neighbors {
                if !scc_set.contains(nxt) {
                    continue;
                }
                if nxt == start && path.len() >= 2 {
                    let mut out = path;
                    out.push(start.to_string());
                    return out;
                }
                if visited.insert(nxt.clone()) {
                    let mut next_path = path.clone();
                    next_path.push(nxt.clone());
                    queue.push_back((nxt.clone(), next_path));
                }
            }
        }
    }
    let mut fallback: Vec<String> = scc_set.iter().cloned().collect();
    fallback.push(start.to_string());
    fallback
}

pub fn check_heavy_import_leak(
    index: &ProgramIndex,
    severity: CheckSeverity,
    heavy_modules: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    if heavy_modules.is_empty() {
        return findings;
    }

    let (targets_py315, requires_py_raw) =
        inspect_project_requires_python_315(&index.meta.project_root);

    // Build runtime top-level import adjacency (`importer -> Vec<&ImportEdge>`)
    let mut adj: BTreeMap<String, Vec<&ImportEdge>> = BTreeMap::new();
    for imp in &index.imports {
        if !imp.is_type_checking && !imp.is_lazy {
            adj.entry(imp.importer_module.clone())
                .or_default()
                .push(imp);
        }
    }

    let is_heavy = |target: &str| -> Option<String> {
        let top = target.split('.').next().unwrap_or(target);
        if heavy_modules.contains(target) {
            Some(target.to_string())
        } else if heavy_modules.contains(top) {
            Some(top.to_string())
        } else {
            None
        }
    };

    // Check from every entry-point module (or all indexed modules if none are marked entry-point)
    let mut root_modules: BTreeSet<String> = index
        .entities
        .iter()
        .filter(|e| e.is_entry_point && !crate::workspace::is_test_path(&e.file, &e.module))
        .map(|e| e.module.clone())
        .collect();
    if root_modules.is_empty() {
        for m in &index.meta.modules {
            if !crate::workspace::is_test_path(&m.file, &m.module_name) {
                root_modules.insert(m.module_name.clone());
            }
        }
    }

    for root_mod in root_modules {
        let mut queue: VecDeque<(String, Vec<String>, Option<&ImportEdge>)> = VecDeque::new();
        let mut visited = BTreeSet::new();
        visited.insert(root_mod.clone());
        queue.push_back((root_mod.clone(), vec![root_mod.clone()], None));

        let mut reported_heavy = BTreeSet::new();
        while let Some((cur, chain, first_imp)) = queue.pop_front() {
            let Some(edges) = adj.get(&cur) else {
                continue;
            };
            for &imp in edges {
                let root_imp = first_imp.unwrap_or(imp);
                let mut next_chain = chain.clone();
                next_chain.push(imp.target_module.clone());

                if let Some(heavy_pkg) = is_heavy(&imp.target_module) {
                    if reported_heavy.insert(heavy_pkg.clone()) {
                        let lazy_guidance = if targets_py315 {
                            format!(
                                "use Python 3.15's `lazy import {heavy_pkg}` statement (https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports) or move the import inside functions"
                            )
                        } else if let Some(ref req) = requires_py_raw {
                            format!(
                                "use Python 3.15's `lazy import {heavy_pkg}` statement (https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports — consider upgrading to Python 3.15 from `requires-python = \"{req}\"` in `pyproject.toml`), or move the import inside functions"
                            )
                        } else {
                            format!(
                                "use Python 3.15's `lazy import {heavy_pkg}` statement (https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports — consider upgrading to Python 3.15 via `requires-python = \">=3.15\"` in `pyproject.toml`), or move the import inside functions"
                            )
                        };
                        findings.push(Finding::new(
                            "heavy-import-leak",
                            "heavy_top_level_import",
                            severity,
                            &root_imp.file,
                            root_imp.line,
                            root_imp.column,
                            format!("{root_mod}.<module>"),
                            vec![root_mod.clone(), heavy_pkg.clone()],
                            format!(
                                "Module '{}' transitively imports heavyweight package '{}' at module load time (chain: {}); {}",
                                root_mod,
                                heavy_pkg,
                                next_chain.join(" -> "),
                                lazy_guidance
                            ),
                            None,
                        ));
                    }
                    continue;
                }

                if next_chain.len() <= 12 && visited.insert(imp.target_module.clone()) {
                    queue.push_back((imp.target_module.clone(), next_chain, Some(root_imp)));
                }
            }
        }
    }

    findings
}

/// Inspects `requires-python` in `pyproject.toml` under `project_root` to determine whether
/// the project already requires Python >= 3.15.
pub fn inspect_project_requires_python_315(project_root: &str) -> (bool, Option<String>) {
    use std::fs;
    use std::path::{Path, PathBuf};

    if project_root.is_empty() {
        return (false, None);
    }
    let candidates = [
        PathBuf::from(project_root),
        PathBuf::from(format!("/{}", project_root.trim_start_matches('/'))),
    ];
    for dir in &candidates {
        let pyproject = Path::new(dir).join("pyproject.toml");
        if pyproject.is_file() {
            if let Ok(content) = fs::read_to_string(&pyproject) {
                if let Ok(val) = content.parse::<toml::Value>() {
                    if let Some(req_str) = val
                        .get("project")
                        .and_then(|p| p.get("requires-python"))
                        .and_then(|r| r.as_str())
                    {
                        let trimmed = req_str.trim().to_string();
                        let is_315 = requires_python_is_at_least_3_15(&trimmed);
                        return (is_315, Some(trimmed));
                    }
                }
            }
            return (false, None);
        }
    }
    (false, None)
}

pub fn requires_python_is_at_least_3_15(spec: &str) -> bool {
    // Check lower-bound version specifiers in `requires-python` (e.g. `>=3.15`, `==3.15.*`, `~=3.15`, `>3.14`)
    for clause in spec.split(',') {
        let c = clause.trim();
        if let Some(ver) = c
            .strip_prefix(">=")
            .or_else(|| c.strip_prefix("~="))
            .or_else(|| c.strip_prefix("=="))
        {
            if let Some((maj, min)) = parse_major_minor(ver.trim()) {
                if maj > 3 || (maj == 3 && min >= 15) {
                    return true;
                }
            }
        } else if let Some(ver) = c.strip_prefix('>') {
            if let Some((maj, min)) = parse_major_minor(ver.trim()) {
                if maj > 3 || (maj == 3 && min >= 14) {
                    return true;
                }
            }
        }
    }
    false
}

fn parse_major_minor(ver: &str) -> Option<(u32, u32)> {
    let mut parts = ver.split('.');
    let maj_str: String = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let min_str: String = parts
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let maj = maj_str.parse::<u32>().ok()?;
    let min = if min_str.is_empty() {
        0
    } else {
        min_str.parse::<u32>().ok()?
    };
    Some((maj, min))
}
