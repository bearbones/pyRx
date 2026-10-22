//! Reproducible static graph depth spot-check. Does not execute Python or write a cache.
//! cargo run --release --example depth_profile -- /path/to/project ...
use pyrx::{
    bake_program_index,
    model::CallEdgeKind,
    query::is_blocking_callee,
    workspace::{SourceSelectionOptions, is_test_path},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn main() {
    for selection in std::env::args().skip(1) {
        let (project, source) = selection.split_once("::").unwrap_or((&selection, ""));
        let started = std::time::Instant::now();
        let index = bake_program_index(
            &SourceSelectionOptions {
                project_root: Some(project.into()),
                sources: if source.is_empty() {
                    vec![]
                } else {
                    vec![source.into()]
                },
                no_cache: true,
                ..Default::default()
            },
            Vec::new(),
            0,
        )
        .expect("index project");
        let indexed_seconds = started.elapsed().as_secs_f64();
        let mut root_depths = Vec::new();
        let mut hazards = BTreeMap::<usize, usize>::new();
        let mut deepest_path = Vec::new();
        for root in &index.entities {
            if (!root.is_entry_point && !root.kind.is_async())
                || is_test_path(&root.file, &root.module)
            {
                continue;
            }
            let mut queue = VecDeque::from([(root.qualified_name.clone(), 0usize)]);
            let mut seen = BTreeSet::from([root.qualified_name.clone()]);
            let mut parents = BTreeMap::new();
            let mut reported_hazards = BTreeSet::new();
            let mut max_depth = 0;
            while let Some((cur, depth)) = queue.pop_front() {
                for edge in index.effective_callees(&cur) {
                    if edge.kind == CallEdgeKind::HigherOrderCallback {
                        continue;
                    }
                    if !index.entity_by_qname.contains_key(&edge.callee)
                        && is_blocking_callee(&edge.callee).is_some()
                        && reported_hazards.insert(edge.callee.clone())
                    {
                        *hazards.entry(depth + 1).or_default() += 1;
                        if depth + 1 > deepest_path.len() {
                            let mut path = pyrx::check::evidence::forward_path(&parents, &cur);
                            path.push(edge.clone());
                            deepest_path = path;
                        }
                    }
                    if index.entity_by_qname.contains_key(&edge.callee)
                        && seen.insert(edge.callee.clone())
                    {
                        max_depth = max_depth.max(depth + 1);
                        queue.push_back((edge.callee.clone(), depth + 1));
                        parents.insert(edge.callee.clone(), edge);
                    }
                }
            }
            root_depths.push(max_depth);
        }
        root_depths.sort();
        let percentile = |p: usize| {
            root_depths
                .get(root_depths.len().saturating_sub(1) * p / 100)
                .copied()
                .unwrap_or(0)
        };
        println!(
            "{}",
            serde_json::json!({
                "project": project, "source": source, "modules": index.meta.modules.len(), "entities": index.entities.len(),
                "calls": index.calls.len(), "roots": root_depths.len(), "rootMaxDepthP50": percentile(50),
                "rootMaxDepthP95": percentile(95), "rootMaxDepthP99": percentile(99), "maxShortestInternalDepth": percentile(100),
                "blockingDepthHistogram": hazards, "deepestBlockingPath": deepest_path,
                "indexSeconds": indexed_seconds, "totalSeconds": started.elapsed().as_secs_f64(),
                "failedModules": index.meta.modules.iter().filter(|m| m.outcome != pyrx::model::ParseOutcome::Indexed).count(),
                "scope": "shortest static paths from non-test entry points and async functions; callbacks excluded; not runtime stack measurements"
            })
        );
    }
}
