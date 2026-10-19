use std::fs;
use std::path::PathBuf;

use pyrx::graph::ProgramIndex;
use pyrx::indexer::index_module;
use pyrx::morph::{MorphRule, run_consolidate, run_declarative_rules};
use pyrx::workspace::{SourceSelectionOptions, select_sources};

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

#[test]
fn test_morph_consolidate_duplicates_across_modules() {
    let root = create_temp_workspace("morph_consolidate");
    let pkg = root.join("src").join("demo");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    // Canonical module
    fs::write(
        pkg.join("common.py"),
        r#"def sanitize_header(raw_val: str) -> str:
    stripped = raw_val.strip()
    if not stripped:
        return "empty"
    parts = stripped.split(":")
    return "-".join(parts)
"#,
    )
    .unwrap();

    // Duplicate module with local callers
    fs::write(
        pkg.join("feature_b.py"),
        r#"def clean_header_value(input_str: str) -> str:
    trimmed = input_str.strip()
    if not trimmed:
        return "empty"
    items = trimmed.split(":")
    return "-".join(items)

def handle_request(hdr: str) -> str:
    return clean_header_value(hdr)
"#,
    )
    .unwrap();

    // Another caller module importing from feature_b
    fs::write(
        pkg.join("routes.py"),
        r#"from demo.feature_b import clean_header_value

def route_entry(h: str) -> str:
    return clean_header_value(h)
"#,
    )
    .unwrap();

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

    // 1. Dry-run first
    let dry_report =
        run_consolidate(&idx, &root, Some("demo.common.sanitize_header"), &[], true).unwrap();
    assert!(dry_report.dry_run);
    assert!(dry_report.files_changed >= 1);
    assert!(dry_report.total_edits >= 1);

    // Verify files on disk were NOT modified during dry-run
    let fb_before = fs::read_to_string(pkg.join("feature_b.py")).unwrap();
    assert!(fb_before.contains("def clean_header_value"));

    // 2. Apply consolidation in-place
    let apply_report =
        run_consolidate(&idx, &root, Some("demo.common.sanitize_header"), &[], false).unwrap();
    assert!(!apply_report.dry_run);
    assert!(apply_report.files_changed >= 1);

    let fb_after = fs::read_to_string(pkg.join("feature_b.py")).unwrap();
    assert!(
        !fb_after.contains("def clean_header_value"),
        "Duplicate function definition should be deleted from feature_b.py, got:\n{fb_after}"
    );
    assert!(
        fb_after.contains("from demo.common import sanitize_header"),
        "Expected import of canonical function in feature_b.py, got:\n{fb_after}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_morph_declarative_rules_conditioned_on_megascope_facts() {
    let root = create_temp_workspace("morph_rules");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("worker.py"),
        r#"import subprocess

def sync_runner(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True)

async def async_runner(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True)
"#,
    )
    .unwrap();

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

    let rules = vec![MorphRule {
        id: "async-subprocess-to-thread".to_string(),
        description: Some(
            "Wrap blocking subprocess.run in asyncio.to_thread inside async def".to_string(),
        ),
        match_callee: "subprocess.run".to_string(),
        when_caller_execution_context: Some("async".to_string()),
        when_reachable_from_async: None,
        action: "wrap_asyncio_to_thread".to_string(),
        replacement_callee: None,
        wrapper_class: None,
        unpack_dict_arg: None,
        template: None,
        ensure_import: Some("import asyncio".to_string()),
    }];

    let report = run_declarative_rules(&idx, &root, &rules, false).unwrap();
    assert_eq!(report.files_changed, 1);

    let updated = fs::read_to_string(pkg.join("worker.py")).unwrap();
    assert!(
        updated.contains("import asyncio"),
        "Expected 'import asyncio' to be added, got:\n{updated}"
    );
    assert!(
        updated.contains("await asyncio.to_thread(subprocess.run, cmd, check=True)"),
        "Expected async call site to be wrapped in await asyncio.to_thread(...), got:\n{updated}"
    );
    // And verify the synchronous caller was NOT wrapped!
    assert!(
        updated.contains(
            "def sync_runner(cmd: list[str]) -> None:\n    subprocess.run(cmd, check=True)"
        ),
        "Synchronous caller must remain untouched, got:\n{updated}"
    );

    let _ = fs::remove_dir_all(&root);
}
