use std::fs;
use std::path::PathBuf;
use std::process::Command;

use pyrx::diff::compute_semantic_diff;
use pyrx::graph::ProgramIndex;
use pyrx::indexer::index_module;
use pyrx::model::{ExitCode, IndexFreshness};
use pyrx::query::{REGISTERED_TOOLS, execute_tool};
use pyrx::snapshot::{load_snapshot, merge_snapshots, save_snapshot};
use pyrx::workspace::{SourceSelectionOptions, select_sources};
use serde_json::{Map, Value, json};

fn create_temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pyrex_test_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build_sample_repo() -> PathBuf {
    let root = create_temp_workspace("megascope_sample");
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "sample_app"
version = "0.1.0"

[project.scripts]
sample-cli = "sample_app.api:main_entry"
"#,
    )
    .unwrap();

    let pkg = root.join("src").join("sample_app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("__init__.py"), "__all__ = ['public_api']\n").unwrap();

    fs::write(
        pkg.join("utils.py"),
        r#"import subprocess
import time

def run_cmd(cmd: list[str]) -> str:
    if not cmd:
        raise ValueError("empty command")
    res = subprocess.run(cmd, check=True)
    return str(res)

def blocking_sleep() -> None:
    time.sleep(0.1)

def orphan_helper_a(x: int) -> int:
    return orphan_helper_b(x + 1)

def orphan_helper_b(y: int) -> int:
    if y > 10:
        return orphan_helper_a(y - 2)
    return y
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("service.py"),
        r#"import threading
from .utils import run_cmd, blocking_sleep

_LOCK = threading.Lock()

class BaseProcessor:
    def process(self, items: list[str]) -> str:
        raise NotImplementedError

class ConcreteProcessor(BaseProcessor):
    def process(self, items: list[str]) -> str:
        with _LOCK:
            return run_cmd(items)

async def async_endpoint(items: list[str]) -> str:
    blocking_sleep()
    proc = ConcreteProcessor()
    return proc.process(items)
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("api.py"),
        r#"from .service import ConcreteProcessor, async_endpoint

def main_entry() -> None:
    proc = ConcreteProcessor()
    try:
        proc.process(["echo", "hi"])
    except ValueError:
        pass

def overloaded_name() -> int:
    return 1
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("extra.py"),
        r#"def overloaded_name() -> int:
    return 2
"#,
    )
    .unwrap();

    root
}

fn make_args(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn test_snapshot_determinism_and_shard_merge() {
    let root = build_sample_repo();
    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let (ws, modules) = select_sources(&sel).unwrap();
    assert_eq!(modules.len(), 5);

    // 1. Forward vs reverse module order determinism
    let shards_fwd: Vec<_> = modules.iter().map(|m| index_module(m, &ws)).collect();
    let mut shards_rev = shards_fwd.clone();
    shards_rev.reverse();

    let idx_fwd = ProgramIndex::from_shards(
        shards_fwd.clone(),
        ws.project_root.to_string_lossy().to_string(),
        ws.entry_points.clone(),
        Vec::new(),
    );
    let idx_rev = ProgramIndex::from_shards(
        shards_rev,
        ws.project_root.to_string_lossy().to_string(),
        ws.entry_points.clone(),
        Vec::new(),
    );

    let snap_t1 = root.join("t1.pyrx");
    let snap_t4 = root.join("t4.pyrx");
    save_snapshot(&snap_t1, &idx_fwd).unwrap();
    save_snapshot(&snap_t4, &idx_rev).unwrap();

    let bytes_t1 = fs::read(&snap_t1).unwrap();
    let bytes_t4 = fs::read(&snap_t4).unwrap();
    assert_eq!(
        bytes_t1, bytes_t4,
        "Snapshots built from different shard orderings must be byte-for-byte identical"
    );

    // Also test CLI --threads 1 vs --threads 4 determinism
    let bin = env!("CARGO_BIN_EXE_pyrx");
    let cli_t1 = root.join("cli_t1.pyrx");
    let cli_t4 = root.join("cli_t4.pyrx");
    let s1 = Command::new(bin)
        .args([
            "megascope",
            "index",
            "--project",
            root.to_str().unwrap(),
            "--threads",
            "1",
            "--output",
            cli_t1.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(s1.success());
    let s4 = Command::new(bin)
        .args([
            "megascope",
            "index",
            "--project",
            root.to_str().unwrap(),
            "--threads",
            "4",
            "--output",
            cli_t4.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(s4.success());
    assert_eq!(
        fs::read(&cli_t1).unwrap(),
        fs::read(&cli_t4).unwrap(),
        "CLI --threads 1 and --threads 4 snapshots must be byte-for-byte identical"
    );

    // 2. Map-reduce shard merge vs single-step index determinism (with diamond overlap)
    let shard_a = ProgramIndex::from_shards(
        shards_fwd[0..3].to_vec(),
        ws.project_root.to_string_lossy().to_string(),
        ws.entry_points.clone(),
        Vec::new(),
    );
    let shard_b = ProgramIndex::from_shards(
        shards_fwd[2..5].to_vec(), // intentional overlap at index 2 (diamond dependency)
        ws.project_root.to_string_lossy().to_string(),
        ws.entry_points.clone(),
        Vec::new(),
    );
    let snap_a = root.join("shard_a.pyrx");
    let snap_b = root.join("shard_b.pyrx");
    let snap_merged = root.join("merged.pyrx");
    save_snapshot(&snap_a, &shard_a).unwrap();
    save_snapshot(&snap_b, &shard_b).unwrap();

    let merged_idx = merge_snapshots(&[snap_a, snap_b], None, &[]).unwrap();
    save_snapshot(&snap_merged, &merged_idx).unwrap();

    let bytes_merged = fs::read(&snap_merged).unwrap();
    assert_eq!(
        bytes_t1, bytes_merged,
        "Merged shard snapshot must be byte-for-byte identical to whole-repo snapshot"
    );

    // 3. Corrupted snapshot checksum detection
    let mut corrupted = bytes_t1.clone();
    let last_idx = corrupted.len() - 1;
    corrupted[last_idx] ^= 0xFF;
    let snap_corrupt = root.join("corrupt.pyrx");
    fs::write(&snap_corrupt, &corrupted).unwrap();
    let err = load_snapshot(&snap_corrupt).unwrap_err();
    assert!(
        err.to_ascii_lowercase().contains("checksum mismatch"),
        "Expected checksum mismatch error, got: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_all_16_megascope_queries_and_contracts() {
    assert!(REGISTERED_TOOLS.len() >= 16);
    let root = build_sample_repo();
    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let (ws, modules) = select_sources(&sel).unwrap();
    let shards: Vec<_> = modules.iter().map(|m| index_module(m, &ws)).collect();
    let idx = ProgramIndex::from_shards(
        shards,
        ws.project_root.to_string_lossy().to_string(),
        ws.entry_points.clone(),
        Vec::new(),
    );

    // 1. get_callers
    let (res, code) = execute_tool(
        "get-callers",
        &make_args(json!({"symbol": "sample_app.utils.run_cmd"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(res["status"], "ok");
    assert_eq!(res["indexScope"]["indexed"], 5);
    assert_eq!(res["indexScope"]["complete"], true);
    let callers = res["callers"].as_array().unwrap();
    assert!(
        callers
            .iter()
            .any(|c| c["caller"] == "sample_app.service.ConcreteProcessor.process")
    );

    // 2. get_callees
    let (res, code) = execute_tool(
        "get-callees",
        &make_args(json!({"symbol": "sample_app.service.ConcreteProcessor.process"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(res["status"], "ok");
    let callees = res["callees"].as_array().unwrap();
    assert!(
        callees
            .iter()
            .any(|c| c["callee"] == "sample_app.utils.run_cmd")
    );

    // 3. get_call_paths
    let (res, code) = execute_tool(
        "get-call-paths",
        &make_args(json!({
            "source": "sample_app.api.main_entry",
            "target": "sample_app.utils.run_cmd",
            "maxDepth": 6
        })),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["paths"].as_array().unwrap().is_empty());

    // 4. get_call_hierarchy (MRO class hierarchy)
    let (res_cls, code_cls) = execute_tool(
        "get-call-hierarchy",
        &make_args(json!({"symbol": "sample_app.service.BaseProcessor"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code_cls, ExitCode::Success);
    assert!(
        res_cls["derivedClasses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str() == Some("sample_app.service.ConcreteProcessor"))
    );

    // 5. get_import_graph
    let (res, code) = execute_tool(
        "get-import-graph",
        &make_args(json!({"module": "sample_app.service"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["imports"].as_array().unwrap().is_empty());

    // 6. get_import_chains
    let (res, code) = execute_tool(
        "get-import-chains",
        &make_args(json!({
            "source": "sample_app.api",
            "target": "sample_app.utils",
            "maxDepth": 5
        })),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["chains"].as_array().unwrap().is_empty());

    // 7. query_throw_propagation
    let (res, code) = execute_tool(
        "query-throw-propagation",
        &make_args(json!({"symbol": "sample_app.utils.run_cmd", "maxDepth": 5})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    let paths = res["paths"].as_array().unwrap();
    assert!(!paths.is_empty());

    // 8. query_nearest_catches
    let (res, code) = execute_tool(
        "query-nearest-catches",
        &make_args(json!({"symbol": "sample_app.utils.run_cmd", "maxDepth": 5})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["catches"].as_array().unwrap().is_empty());

    // 9. query_async_hazards
    let (res, code) = execute_tool(
        "query-async-hazards",
        &make_args(json!({"symbol": "sample_app.service.async_endpoint"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["hazards"].as_array().unwrap().is_empty());

    // 10. query_context_managers
    let (res, code) = execute_tool(
        "query-context-managers",
        &make_args(json!({"symbol": "sample_app.service.ConcreteProcessor.process"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(!res["sites"].as_array().unwrap().is_empty());

    // 11. analyze_dead_code (detects mutually recursive orphan_helper_a <-> orphan_helper_b)
    let (res, code) = execute_tool(
        "analyze-dead-code",
        &make_args(json!({})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    let dead = res["dead"].as_array().unwrap();
    let dead_qnames: Vec<_> = dead
        .iter()
        .filter_map(|d| d["qualifiedName"].as_str())
        .collect();
    assert!(dead_qnames.contains(&"sample_app.utils.orphan_helper_a"));
    assert!(dead_qnames.contains(&"sample_app.utils.orphan_helper_b"));

    // 12. analyze_change_impact
    let (res, code) = execute_tool(
        "analyze-change-impact",
        &make_args(json!({"symbol": "sample_app.utils.run_cmd", "maxDepth": 5})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    let affected_eps = res["affectedEntryPoints"].as_array().unwrap();
    assert!(
        affected_eps
            .iter()
            .any(|e| e.as_str() == Some("sample_app.api.main_entry"))
    );

    // 13. search_functions
    let (res, code) = execute_tool(
        "search-functions",
        &make_args(json!({"pattern": "run_cmd"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(res["matches"].as_array().unwrap().len(), 1);

    // 14. lookup_function
    let (res, code) = execute_tool(
        "lookup-function",
        &make_args(json!({"symbol": "sample_app.utils.run_cmd"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(res["qualifiedName"], "sample_app.utils.run_cmd");

    // 15. list_entry_points
    let (res, code) = execute_tool(
        "list-entry-points",
        &make_args(json!({})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert!(
        res["entryPoints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["qualifiedName"] == "sample_app.api.main_entry")
    );

    // 16. graph_summary
    let (res, code) = execute_tool(
        "graph-summary",
        &make_args(json!({})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(res["modules"], 5);

    // Contract: NotFound with didYouMean
    let (nf_res, nf_code) = execute_tool(
        "lookup-function",
        &make_args(json!({"symbol": "sample_app.utils.run_cmdd"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(nf_code, ExitCode::FindingsOrEmpty);
    assert_eq!(nf_res["status"], "not_found");
    assert!(
        nf_res["didYouMean"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["qualifiedName"].as_str().unwrap().contains("run_cmd"))
    );

    // Contract: Ambiguous with candidates
    let (amb_res, amb_code) = execute_tool(
        "lookup-function",
        &make_args(json!({"symbol": "overloaded_name"})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(amb_code, ExitCode::Ambiguous);
    assert_eq!(amb_res["status"], "ambiguous");
    assert_eq!(amb_res["candidates"].as_array().unwrap().len(), 2);

    // Contract: Paging
    let (page_res, page_code) = execute_tool(
        "search-functions",
        &make_args(json!({"pattern": "", "limit": 2, "offset": 0})),
        &idx,
        IndexFreshness::Baked,
    );
    assert_eq!(page_code, ExitCode::Success);
    assert_eq!(page_res["matches"].as_array().unwrap().len(), 2);
    assert_eq!(page_res["hasMore"], true);
    assert_eq!(page_res["nextOffset"], 2);
    assert_eq!(page_res["truncated"], true);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_snapshot_diff_and_cli_exit_codes() {
    let root = build_sample_repo();
    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let (ws1, modules1) = select_sources(&sel).unwrap();
    let shards1: Vec<_> = modules1.iter().map(|m| index_module(m, &ws1)).collect();
    let idx1 = ProgramIndex::from_shards(
        shards1,
        ws1.project_root.to_string_lossy().to_string(),
        ws1.entry_points.clone(),
        Vec::new(),
    );

    // Modify utils.py to add a new function and modify run_cmd
    fs::write(
        root.join("src/sample_app/utils.py"),
        r#"import subprocess

def run_cmd(cmd: list[str]) -> str:
    res = subprocess.run(cmd, check=False)
    return str(res)

def newly_added_fn() -> int:
    return 42
"#,
    )
    .unwrap();

    let (ws2, modules2) = select_sources(&sel).unwrap();
    let shards2: Vec<_> = modules2.iter().map(|m| index_module(m, &ws2)).collect();
    let idx2 = ProgramIndex::from_shards(
        shards2,
        ws2.project_root.to_string_lossy().to_string(),
        ws2.entry_points.clone(),
        Vec::new(),
    );

    let diff = compute_semantic_diff(&idx1, &idx2, true, 5);
    assert!(
        diff.added_entities
            .contains(&"sample_app.utils.newly_added_fn".to_string())
    );
    assert!(
        diff.removed_entities
            .contains(&"sample_app.utils.orphan_helper_a".to_string())
    );
    assert!(
        diff.modified_entities
            .contains(&"sample_app.utils.run_cmd".to_string())
    );
    assert!(
        diff.impact
            .as_ref()
            .unwrap()
            .affected_entry_points
            .contains(&"sample_app.api.main_entry".to_string())
    );

    // Test CLI exit code 4 on ambiguous symbol
    let snap_path = root.join("test.pyrx");
    save_snapshot(&snap_path, &idx1).unwrap();

    let bin = env!("CARGO_BIN_EXE_pyrx");
    let out = Command::new(bin)
        .args([
            "megascope",
            "lookup-function",
            "--index",
            snap_path.to_str().unwrap(),
            "--symbol",
            "overloaded_name",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(ExitCode::Ambiguous as i32));

    let _ = fs::remove_dir_all(&root);
}
