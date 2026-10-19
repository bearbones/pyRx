use crate::check::{CheckSeverity, Finding};
use crate::graph::ProgramIndex;
use crate::model::*;
use crate::query::is_blocking_callee;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn non_blocking_replacement_hint(callee: &str, hazard_kind: &str) -> &'static str {
    match hazard_kind {
        "blocking_sleep" => "use `await asyncio.sleep(...)` instead of `time.sleep(...)`",
        "blocking_file_io" => {
            if callee == "open" {
                "use `async with aiofiles.open(...)` or `await asyncio.to_thread(open, ...)` instead of synchronous `open(...)`"
            } else {
                "use `aiofiles` or wrap synchronous file I/O in `await asyncio.to_thread(...)`"
            }
        }
        "blocking_network_io" => {
            "use `async with httpx.AsyncClient() as client: await client.get(...)` (or `aiohttp.ClientSession`) instead of synchronous `requests`/`urllib` calls"
        }
        "blocking_subprocess" => {
            "use `await asyncio.create_subprocess_exec(...)` or `await asyncio.to_thread(subprocess.run, ...)` instead of synchronous subprocess execution"
        }
        "blocking_db_io" => {
            "use an async database driver (`aiosqlite`, `asyncpg`, `psycopg.AsyncConnection`) or `await asyncio.to_thread(...)`"
        }
        _ => "offload blocking work with `await asyncio.to_thread(...)`",
    }
}

pub fn check_blocking_in_async(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();

    for root in &index.entities {
        if !root.kind.is_async() && root.execution_context != ExecutionContext::Async {
            continue;
        }

        let mut queue: VecDeque<(String, Vec<String>, Option<CallEdge>)> = VecDeque::new();
        let mut visited = BTreeSet::new();
        let mut reported_callees = BTreeSet::new();
        visited.insert(root.qualified_name.clone());
        queue.push_back((
            root.qualified_name.clone(),
            vec![root.qualified_name.clone()],
            None,
        ));

        while let Some((cur, path, first_edge)) = queue.pop_front() {
            if path.len() > 8 {
                continue;
            }
            for edge in index.effective_callees(&cur) {
                if edge.kind == CallEdgeKind::HigherOrderCallback {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(edge.callee.clone());
                let root_edge = first_edge.clone().unwrap_or_else(|| edge.clone());

                if let Some(hazard_kind) = is_blocking_callee(&edge.callee) {
                    if reported_callees.insert(edge.callee.clone()) {
                        let hops = next_path.len() - 1;
                        let replacement = non_blocking_replacement_hint(&edge.callee, hazard_kind);
                        findings.push(Finding::new(
                            "blocking-in-async",
                            hazard_kind,
                            severity,
                            &root_edge.file,
                            root_edge.line,
                            root_edge.column,
                            &root.qualified_name,
                            vec![root.qualified_name.clone(), edge.callee.clone()],
                            format!(
                                "Async coroutine '{}' transitively calls blocking '{}' ({} hop(s): {}); {}",
                                root.qualified_name,
                                edge.callee,
                                hops,
                                next_path.join(" -> "),
                                replacement
                            ),
                            None,
                        ));
                    }
                    continue;
                }

                if index.entity_by_qname.contains_key(&edge.callee)
                    && visited.insert(edge.callee.clone())
                {
                    queue.push_back((edge.callee, next_path, Some(root_edge)));
                }
            }
        }
    }

    findings
}

#[derive(Clone, Debug)]
struct LockOrderEdge {
    from_lock: String,
    to_lock: String,
    caller: String,
    file: String,
    line: u32,
    detail: String,
}

pub fn check_lock_order_inversion(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();

    // 1. Report `await` while holding a `threading.Lock` / `RLock`
    for (caller, lock_id, line, col) in &index.await_under_threading_lock {
        let file = index
            .entity_by_qname
            .get(caller)
            .map(|&i| index.entities[i].file.clone())
            .unwrap_or_default();
        findings.push(Finding::new(
            "lock-order-inversion",
            "await_under_threading_lock",
            severity,
            file,
            *line,
            *col,
            caller.clone(),
            vec![caller.clone(), lock_id.clone()],
            format!(
                "Coroutine '{}' executes 'await' while holding synchronous threading lock '{}'",
                caller, lock_id
            ),
            None,
        ));
    }

    // 2. Compute transitive lock acquisitions for every entity (up to 6 hops)
    let mut transitive_locks: BTreeMap<String, Vec<(String, String, u32)>> = BTreeMap::new();
    for ent in &index.entities {
        if ent.kind.is_callable() {
            let locks = collect_transitive_locks(&ent.qualified_name, index, 6);
            if !locks.is_empty() {
                transitive_locks.insert(ent.qualified_name.clone(), locks);
            }
        }
    }

    // 3. Build lock ordering edges (L1 -> L2 when L2 is acquired while L1 is held)
    let mut order_edges: Vec<LockOrderEdge> = Vec::new();

    // 3a. Nested `with lock_a: with lock_b:` or call sites under `with lock_a:`
    for ctx in &index.contexts {
        let held_locks: Vec<&WithScope> = ctx
            .with_scopes
            .iter()
            .filter(|w| w.lock_identity.is_some())
            .collect();

        // Nested locks active simultaneously in `ctx.with_scopes`
        for i in 0..held_locks.len() {
            for j in (i + 1)..held_locks.len() {
                let l1 = held_locks[i].lock_identity.as_ref().unwrap();
                let l2 = held_locks[j].lock_identity.as_ref().unwrap();
                if l1 != l2 {
                    order_edges.push(LockOrderEdge {
                        from_lock: l1.clone(),
                        to_lock: l2.clone(),
                        caller: ctx.caller.clone(),
                        file: ctx.file.clone(),
                        line: held_locks[j].line,
                        detail: format!(
                            "'{}' acquires '{}' while holding '{}'",
                            ctx.caller, l2, l1
                        ),
                    });
                }
            }
        }

        // Interprocedural: `ctx` holds `l1` and calls `ctx.callee`, which transitively acquires `l2`
        if !held_locks.is_empty() {
            if let Some(callee_locks) = transitive_locks.get(&ctx.callee) {
                for w in &held_locks {
                    let l1 = w.lock_identity.as_ref().unwrap();
                    for (l2, acq_fn, _acq_line) in callee_locks {
                        if l1 != l2 {
                            order_edges.push(LockOrderEdge {
                                from_lock: l1.clone(),
                                to_lock: l2.clone(),
                                caller: ctx.caller.clone(),
                                file: ctx.file.clone(),
                                line: ctx.line,
                                detail: format!(
                                    "'{}' holds '{}' and calls '{}' (which acquires '{}' in '{}')",
                                    ctx.caller, l1, ctx.callee, l2, acq_fn
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    // Check for cycles in `order_edges` (L1 -> L2 and L2 -> L1 or multi-step cycle)
    let mut adj: BTreeMap<String, Vec<&LockOrderEdge>> = BTreeMap::new();
    for e in &order_edges {
        adj.entry(e.from_lock.clone()).or_default().push(e);
    }

    let mut reported_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for e1 in &order_edges {
        // Check if there is a path from `e1.to_lock` back to `e1.from_lock`
        if let Some(back_edge) = find_lock_path(&e1.to_lock, &e1.from_lock, &adj) {
            let mut pair_key = [e1.from_lock.clone(), e1.to_lock.clone()];
            pair_key.sort();
            if reported_pairs.insert((pair_key[0].clone(), pair_key[1].clone())) {
                findings.push(Finding::new(
                    "lock-order-inversion",
                    "lock_acquisition_cycle",
                    severity,
                    &e1.file,
                    e1.line,
                    1,
                    &e1.caller,
                    vec![e1.from_lock.clone(), e1.to_lock.clone()],
                    format!(
                        "Lock-order inversion cycle between '{}' and '{}': {} vs {}",
                        e1.from_lock, e1.to_lock, e1.detail, back_edge.detail
                    ),
                    None,
                ));
            }
        }
    }

    findings
}

fn collect_transitive_locks(
    start_qname: &str,
    index: &ProgramIndex,
    max_depth: usize,
) -> Vec<(String, String, u32)> {
    let mut out = Vec::new();
    let mut seen_locks = BTreeSet::new();
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();
    visited.insert(start_qname.to_string());
    queue.push_back((start_qname.to_string(), 0usize));

    while let Some((cur, depth)) = queue.pop_front() {
        if let Some(acqs) = index.lock_acquisitions.get(&cur) {
            for (lock_id, line, _is_async) in acqs {
                if seen_locks.insert(lock_id.clone()) {
                    out.push((lock_id.clone(), cur.clone(), *line));
                }
            }
        }
        if depth >= max_depth {
            continue;
        }
        for edge in index.effective_callees(&cur) {
            if index.entity_by_qname.contains_key(&edge.callee)
                && visited.insert(edge.callee.clone())
            {
                queue.push_back((edge.callee, depth + 1));
            }
        }
    }
    out
}

fn find_lock_path<'a>(
    start: &str,
    target: &str,
    adj: &'a BTreeMap<String, Vec<&'a LockOrderEdge>>,
) -> Option<&'a LockOrderEdge> {
    let mut queue = VecDeque::new();
    let mut visited = BTreeSet::new();
    visited.insert(start.to_string());
    queue.push_back((start.to_string(), None));

    while let Some((cur, first_edge)) = queue.pop_front() {
        if let Some(edges) = adj.get(&cur) {
            for &edge in edges {
                let fe = first_edge.unwrap_or(edge);
                if edge.to_lock == target {
                    return Some(fe);
                }
                if visited.insert(edge.to_lock.clone()) {
                    queue.push_back((edge.to_lock.clone(), Some(fe)));
                }
            }
        }
    }
    None
}

pub fn check_unowned_resource_return(
    index: &ProgramIndex,
    severity: CheckSeverity,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for ent in &index.entities {
        if ent.returned_resources.is_empty() {
            continue;
        }
        // Skip contextmanager-decorated factory functions (`@contextmanager`, `@asynccontextmanager`, `__enter__`)
        if ent.name == "__enter__"
            || ent.name == "__aenter__"
            || ent
                .decorators
                .iter()
                .any(|d| d.contains("contextmanager") || d.contains("asynccontextmanager"))
        {
            continue;
        }

        let callers = index.effective_callers(&ent.qualified_name);
        let mut unowned_callers = Vec::new();
        for edge in &callers {
            let owned_by_caller_with =
                index
                    .contexts
                    .get(edge.context_id as usize)
                    .is_some_and(|ctx| {
                        ctx.with_scopes
                            .iter()
                            .any(|w| w.context_expr.contains(&ent.name) || w.line == edge.line)
                    });
            if !owned_by_caller_with {
                unowned_callers.push(edge.caller.clone());
            }
        }

        if callers.is_empty() || !unowned_callers.is_empty() {
            for alloc in &ent.returned_resources {
                let caller_note = if unowned_callers.is_empty() {
                    "without @contextmanager or ExitStack ownership".to_string()
                } else {
                    format!(
                        "and called without 'with' ownership by: {}",
                        unowned_callers.join(", ")
                    )
                };
                findings.push(Finding::new(
                    "unowned-resource-return",
                    "unowned_resource_return",
                    severity,
                    &ent.file,
                    alloc.line,
                    alloc.column,
                    &ent.qualified_name,
                    vec![ent.qualified_name.clone(), alloc.callee.clone()],
                    format!(
                        "Function '{}' allocates context-managed resource '{}' ({:?}) and returns it {}",
                        ent.qualified_name, alloc.callee, alloc.resource_kind, caller_note
                    ),
                    None,
                ));
            }
        }
    }

    findings
}
