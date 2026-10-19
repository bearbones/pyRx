use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use pyrx::check::{
    ALL_CHECKS, ResolvedCheckConfig, build_baseline_file, render_sarif_report, run_checks,
};
use pyrx::graph::ProgramIndex;
use pyrx::indexer::index_module;
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

fn build_all_16_checks_repo() -> PathBuf {
    let root = create_temp_workspace("check_all_16");
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "check_demo"
version = "0.1.0"

[project.scripts]
demo-cli = "check_demo.entry:run_app"
"#,
    )
    .unwrap();

    let pkg = root.join("src").join("check_demo");
    fs::create_dir_all(&pkg).unwrap();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();

    fs::write(pkg.join("__init__.py"), "").unwrap();

    // 1. semantic-clone & 2. schema-drift & 3. dict-contract-mismatch & 4. env-sprawl
    fs::write(
        pkg.join("sprawl_a.py"),
        r#"import os
from dataclasses import dataclass

@dataclass
class UserProfile:
    user_id: int
    email: str
    role: str
    active: bool

def normalize_slug(raw_input: str) -> str:
    cleaned = raw_input.strip()
    if not cleaned:
        return "default"
    parts = cleaned.split("-")
    return "_".join(parts)

def build_user_payload(uid: int) -> dict:
    timeout = os.getenv("API_TIMEOUT", "30")
    return {"user_id": uid, "status": "active", "timeout": timeout}
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("sprawl_b.py"),
        r#"import os
from typing import TypedDict
from .sprawl_a import build_user_payload

class UserProfileDto(TypedDict):
    user_id: str
    email: str
    role: str
    active: bool

def canonicalize_token(text_val: str) -> str:
    trimmed = text_val.strip()
    if not trimmed:
        return "default"
    segments = trimmed.split("-")
    return "_".join(segments)

def consume_user_payload(uid: int) -> str:
    t = os.environ.get("API_TIMEOUT", "60")
    payload = build_user_payload(uid)
    return str(payload["usr_id"]) + str(t)
"#,
    )
    .unwrap();

    // 5. dead-code & 6. unused-parameter-cross-tu & 7. test-only-production-code
    fs::write(
        pkg.join("reach.py"),
        r#"def dead_cycle_first(x: int) -> int:
    return dead_cycle_second(x + 1)

def dead_cycle_second(y: int) -> int:
    return dead_cycle_first(y - 1)

def active_with_unused_param(used_val: int, legacy_flag: bool) -> int:
    return used_val * 2

def helper_only_called_by_tests(x: int) -> int:
    return x + 99
"#,
    )
    .unwrap();

    // 8. uncaught-exception-escape & 9. broad-catch-swallow & 10. exception-contract-divergence
    fs::write(
        pkg.join("errors.py"),
        r#"class StorageBackend:
    def fetch_record(self, key: str) -> str:
        raise NotImplementedError

class SqlStorage(StorageBackend):
    def fetch_record(self, key: str) -> str:
        if not key:
            raise KeyError("missing key")
        return key

class MemoryStorage(StorageBackend):
    def fetch_record(self, key: str) -> str | None:
        if not key:
            return None
        return key

def swallow_errors() -> int:
    try:
        SqlStorage().fetch_record("")
    except Exception:
        pass
    return 0
"#,
    )
    .unwrap();

    // 11. blocking-in-async & 12. lock-order-inversion & 13. unowned-resource-return
    fs::write(
        pkg.join("concurrency.py"),
        r#"import threading
import time

LOCK_ALPHA = threading.Lock()
LOCK_BETA = threading.Lock()

def acquire_alpha_then_beta() -> int:
    with LOCK_ALPHA:
        with LOCK_BETA:
            return 1

def acquire_beta_then_alpha() -> int:
    with LOCK_BETA:
        with LOCK_ALPHA:
            return 2

def sync_sleep_helper() -> None:
    time.sleep(0.05)

async def handle_request_async() -> int:
    sync_sleep_helper()
    return 42

def open_raw_log_file(path: str):
    return open(path, "w")
"#,
    )
    .unwrap();

    // 14. import-side-effects & 15. import-cycle & 16. heavy-import-leak
    fs::write(
        pkg.join("cycle_x.py"),
        r#"import subprocess
import torch
from .cycle_y import ping_y

subprocess.run(["echo", "side-effect-at-import-time"])

def ping_x() -> int:
    return 1
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("cycle_y.py"),
        r#"from .cycle_x import ping_x

def ping_y() -> int:
    return ping_x()
"#,
    )
    .unwrap();

    // Entry point wiring everything together
    fs::write(
        pkg.join("entry.py"),
        r#"from .sprawl_a import normalize_slug
from .sprawl_b import canonicalize_token, consume_user_payload
from .reach import active_with_unused_param
from .errors import SqlStorage, MemoryStorage, swallow_errors
from .concurrency import (
    acquire_alpha_then_beta,
    acquire_beta_then_alpha,
    handle_request_async,
    open_raw_log_file,
)
from .cycle_y import ping_y

def run_app() -> None:
    normalize_slug("a-b")
    canonicalize_token("c-d")
    consume_user_payload(1)
    active_with_unused_param(10, False)
    active_with_unused_param(20, False)
    SqlStorage().fetch_record("k")
    MemoryStorage().fetch_record("k")
    swallow_errors()
    acquire_alpha_then_beta()
    acquire_beta_then_alpha()
    f = open_raw_log_file("/tmp/demo.log")
    ping_y()
"#,
    )
    .unwrap();

    // Test file calling helper_only_called_by_tests
    fs::write(
        tests_dir.join("test_reach.py"),
        r#"from check_demo.reach import helper_only_called_by_tests

def test_helper() -> None:
    assert helper_only_called_by_tests(1) == 100
"#,
    )
    .unwrap();

    root
}

#[test]
fn test_all_16_checks_fire_accurately() {
    assert_eq!(ALL_CHECKS.len(), 16);
    let root = build_all_16_checks_repo();
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

    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("all")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);

    let triggered_checks: BTreeSet<String> =
        report.findings.iter().map(|f| f.check.clone()).collect();

    for desc in ALL_CHECKS {
        assert!(
            triggered_checks.contains(desc.name),
            "Expected check '{}' ({}) to fire on synthetic repo, but triggered checks were: {:?}",
            desc.name,
            desc.category,
            triggered_checks
        );
    }

    // Verify SARIF 2.1.0 output schema and pyrexFingerprint/v1 presence
    let sarif_str = render_sarif_report(&report);
    let sarif_json: serde_json::Value = serde_json::from_str(&sarif_str).unwrap();
    assert_eq!(sarif_json["version"], "2.1.0");
    let results = sarif_json["runs"][0]["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(
        results[0]["partialFingerprints"]["pyrexFingerprint/v1"]
            .as_str()
            .is_some()
    );

    // Verify baseline generation and filtering
    let baseline = build_baseline_file(&report.findings);
    let baselined_report = run_checks(&idx, &cfg, Some(&baseline), None);
    assert_eq!(
        baselined_report.findings.len(),
        0,
        "All findings should be suppressed when matched against their own baseline"
    );
    assert_eq!(
        baselined_report.summary.baselined,
        Some(report.findings.len())
    );

    // Verify --patch-file diff-scoped filtering
    let first_finding = &report.findings[0];
    let patch = format!(
        "--- a/{file}\n+++ b/{file}\n@@ -{line},1 +{line},1 @@\n+changed line\n",
        file = first_finding.file,
        line = first_finding.line
    );
    let diff_report = run_checks(&idx, &cfg, None, Some(&patch));
    assert!(
        !diff_report.findings.is_empty() && diff_report.findings.len() < report.findings.len(),
        "Diff-scoped check run should only retain findings on lines touched by patch"
    );
    assert!(
        diff_report
            .findings
            .iter()
            .all(|f| f.file == first_finding.file && f.line == first_finding.line)
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_inline_suppressions_and_fingerprint_stability() {
    let root = create_temp_workspace("check_suppressions");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("mod_a.py"),
        r#"__all__ = ["entry_fn"]

def entry_fn() -> int:
    return 1

def suppressed_unused() -> int:  # pyrex: ignore[dead-code]
    return 10

def unsuppressed_unused() -> int:
    return 20
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

    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("dead-code")).unwrap();
    let report1 = run_checks(&idx, &cfg, None, None);
    assert_eq!(report1.summary.suppressed, 1);
    assert_eq!(report1.findings.len(), 1);
    let fp1 = report1.findings[0].fingerprint.clone();

    // Insert 5 blank/comment lines above `unsuppressed_unused` and verify fingerprint stability
    fs::write(
        pkg.join("mod_a.py"),
        r#"# comment line 1
# comment line 2
# comment line 3
# comment line 4
# comment line 5
__all__ = ["entry_fn"]

def entry_fn() -> int:
    return 1

def suppressed_unused() -> int:  # pyrex: ignore[dead-code]
    return 10

def unsuppressed_unused() -> int:
    return 20
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
    let report2 = run_checks(&idx2, &cfg, None, None);
    assert_eq!(report2.findings.len(), 1);
    assert_eq!(
        fp1, report2.findings[0].fingerprint,
        "pyrexFingerprint/v1 must remain stable across line-number shifts"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_exception_contract_divergence_includes_base_contract_location() {
    let root = build_all_16_checks_repo();
    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("exception-contract-divergence"))
            .unwrap();
    let report = run_checks(&idx, &cfg, None, None);
    assert!(!report.findings.is_empty());
    let has_base_loc = report.findings.iter().any(|f| {
        f.check == "exception-contract-divergence"
            && f.message.contains(
                "'check_demo.errors.StorageBackend.fetch_record' (src/check_demo/errors.py:2)",
            )
    });
    assert!(
        has_base_loc,
        "Expected exception-contract-divergence message to include base contract file:line, got: {:?}",
        report.findings
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_dead_code_recognizes_init_reexports_all_and_entry_points() {
    let root = create_temp_workspace("dead_code_exports");
    let pkg = root.join("src").join("mypkg");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"mypkg\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join(".pyrx.toml"),
        r#"entry_points = ["mypkg.configured_ep:plugin_hook"]
"#,
    )
    .unwrap();

    // Package __init__.py re-exports PublicClass and reexported_fn from .api
    fs::write(
        pkg.join("__init__.py"),
        r#"from .api import PublicClass, reexported_fn
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("api.py"),
        r#"class PublicClass:
    def public_method(self) -> int:
        return helper_called_by_public_method()

def helper_called_by_public_method() -> int:
    return 1

def reexported_fn() -> int:
    return 2

def truly_dead_fn() -> int:
    return 3
"#,
    )
    .unwrap();

    // Module using __all__ += and __all__.extend
    fs::write(
        pkg.join("all_mod.py"),
        r#"__all__ = ["exported_one"]
__all__ += ["exported_two"]
__all__.extend(["exported_three"])

def exported_one() -> int:
    return 1

def exported_two() -> int:
    return 2

def exported_three() -> int:
    return 3
"#,
    )
    .unwrap();

    // Module with entry_point configured in .pyrx.toml
    fs::write(
        pkg.join("configured_ep.py"),
        r#"def plugin_hook() -> int:
    return 42
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("dead-code")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);

    let flagged_symbols: Vec<&str> = report.findings.iter().map(|f| f.scope.as_str()).collect();
    assert_eq!(
        flagged_symbols,
        vec!["mypkg.api.truly_dead_fn"],
        "Only truly_dead_fn should be flagged as dead-code; got findings: {:?}",
        report.findings
    );
    let msg = &report.findings[0].message;
    assert!(msg.contains("__all__"));
    assert!(msg.contains("__init__.py"));
    assert!(msg.contains("[entry_points]"));
    assert!(msg.contains("test-only-production-code"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_unused_parameter_and_legacy_alias_and_underscore_guidance() {
    let root = create_temp_workspace("unused_param_test");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "app"
[project.scripts]
run = "app.main:main"
"#,
    )
    .unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();
    fs::write(
        pkg.join("main.py"),
        r#"def helper_bad(x: int, unused_arg: str) -> int:
    return x + 1

def helper_ok(x: int, _compat_arg: str) -> int:
    return x + 2

def helper_suppressed(x: int, dead_arg: str) -> int:  # pyrx: ignore[unused-parameter-cross-tu]
    return x + 3

def main() -> int:
    a = helper_bad(1, "a") + helper_bad(2, "b")
    b = helper_ok(1, "a") + helper_ok(2, "b")
    c = helper_suppressed(1, "a") + helper_suppressed(2, "b")
    return a + b + c
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();

    // Resolve via legacy alias 'unused-parameter-cross-tu'
    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("unused-parameter-cross-tu"))
        .unwrap();
    assert!(cfg.enabled_checks.contains("unused-parameter"));

    let report = run_checks(&idx, &cfg, None, None);
    assert_eq!(report.summary.suppressed, 1);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].check, "unused-parameter");
    assert!(report.findings[0].message.contains("'_unused_arg'"));
    assert!(report.findings[0].message.contains("[[maybe_unused]]"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_uncentralized_env_access_and_config_modules() {
    let root = create_temp_workspace("env_access_test");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(
        root.join(".pyrx.toml"),
        r#"config_modules = ["config", "settings", "app.custom_env_loader"]
"#,
    )
    .unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();
    fs::write(
        pkg.join("config.py"),
        "import os\nDB_URL = os.getenv('DB_URL', 'sqlite://')\n",
    )
    .unwrap();
    fs::write(
        pkg.join("custom_env_loader.py"),
        "import os\nSECRET = os.getenv('SECRET_KEY', 'dev')\n",
    )
    .unwrap();
    fs::write(
        pkg.join("service.py"),
        "import os\ndef handler():\n    return os.getenv('SERVICE_TOKEN', 'none')\n",
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();

    // Test both canonical name and legacy alias 'env-sprawl'
    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("env-sprawl")).unwrap();
    assert!(cfg.enabled_checks.contains("uncentralized-env-access"));

    let report = run_checks(&idx, &cfg, None, None);
    assert_eq!(report.findings.len(), 1);
    let f = &report.findings[0];
    assert_eq!(f.check, "uncentralized-env-access");
    assert_eq!(f.file, "src/app/service.py");
    assert!(f.message.contains("config_modules"));
    assert!(f.message.contains(".pyrx.toml"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_blocking_in_async_replacement_hints() {
    let root = create_temp_workspace("blocking_hints");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();
    fs::write(
        pkg.join("handlers.py"),
        r#"import subprocess
import time
import requests

async def endpoint_sleep():
    time.sleep(1)

async def endpoint_open():
    with open("/tmp/x.txt") as f:
        return f.read()

async def endpoint_http():
    return requests.get("https://example.com")

async def endpoint_subproc():
    return subprocess.run(["ls"])
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("blocking-in-async")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);
    assert_eq!(report.findings.len(), 4);

    let messages: Vec<&str> = report.findings.iter().map(|f| f.message.as_str()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("await asyncio.sleep(...)"))
    );
    assert!(messages.iter().any(|m| m.contains("aiofiles.open")));
    assert!(messages.iter().any(|m| m.contains("httpx.AsyncClient")));
    assert!(
        messages
            .iter()
            .any(|m| m.contains("await asyncio.create_subprocess_exec(...)"))
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_heavy_import_leak_python_315_lazy_import_and_ci_exit_codes() {
    let root = create_temp_workspace("heavy_import_315");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "app"
version = "0.1.0"
requires-python = ">=3.12"
"#,
    )
    .unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();
    fs::write(
        pkg.join("eager_mod.py"),
        r#"import torch

def run() -> int:
    return 1
"#,
    )
    .unwrap();
    // Module using Python 3.15 PEP 810 `lazy import` should NOT trigger heavy-import-leak
    fs::write(
        pkg.join("lazy_mod.py"),
        r#"lazy import tensorflow
lazy from pandas import DataFrame

def run_lazy() -> int:
    return 2
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("heavy-import-leak")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);

    assert_eq!(
        report.findings.len(),
        1,
        "Only eager_mod.py should be flagged, lazy_mod.py uses PEP 810 lazy import; got: {:?}",
        report.findings
    );
    let f = &report.findings[0];
    assert_eq!(f.file, "src/app/eager_mod.py");
    assert!(f.message.contains("lazy import torch"));
    assert!(
        f.message
            .contains("https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports")
    );
    assert!(f.message.contains("consider upgrading to Python 3.15"));

    // Now update requires-python to >=3.15 and verify the message adapts
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "app"
version = "0.1.0"
requires-python = ">=3.15"
"#,
    )
    .unwrap();
    let report_315 = run_checks(&idx, &cfg, None, None);
    assert_eq!(report_315.findings.len(), 1);
    assert!(
        !report_315.findings[0]
            .message
            .contains("consider upgrading to Python 3.15")
    );
    assert!(report_315.findings[0].message.contains("lazy import torch"));

    // Test CLI exit code configurability via .pyrx.toml `fail = false`
    fs::write(
        root.join(".pyrx.toml"),
        r#"checks = ["heavy-import-leak"]

[rules.heavy-import-leak]
severity = "warning"
fail = false
"#,
    )
    .unwrap();

    let pyrx_bin = env!("CARGO_BIN_EXE_pyrx");
    let out = std::process::Command::new(pyrx_bin)
        .args(["check", "--project", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "pyrx check should exit 0 when [rules.heavy-import-leak] fail = false, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout_str.contains("[heavy-import-leak]"),
        "Finding must still be reported in output even when fail = false"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_all_16_check_docs_exist() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let docs_dir = manifest_dir.join("docs").join("checks");
    assert!(
        docs_dir.join("README.md").is_file(),
        "docs/checks/README.md must exist"
    );
    for check in ALL_CHECKS {
        let doc_path = docs_dir.join(format!("{}.md", check.name));
        assert!(
            doc_path.is_file(),
            "Documentation file missing for check '{}': {}",
            check.name,
            doc_path.display()
        );
        let content = fs::read_to_string(&doc_path).unwrap();
        assert!(
            content.contains(check.name) && content.len() > 200,
            "Documentation file '{}' should be substantive",
            doc_path.display()
        );
    }
}

#[test]
fn test_dict_contract_mismatch_ignores_stores_deletes_and_guards() {
    let root = create_temp_workspace("dict_contract_precision");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("producer.py"),
        r#"def make_event() -> dict:
    return {"event_id": 1, "kind": "click"}
"#,
    )
    .unwrap();

    fs::write(
        pkg.join("consumer.py"),
        r#"from .producer import make_event

def enrich_and_read() -> str:
    ev = make_event()
    # 1. Store into a new key should NOT be flagged, nor reading it afterwards
    ev["injected_trace"] = "abc"
    t = ev["injected_trace"]
    # 2. Augmented assignment on an existing or newly written key should NOT be flagged
    ev["event_id"] += 1
    # 3. Guarded read via `in` check should NOT be flagged
    if "optional_meta" in ev:
        m = ev["optional_meta"]
    else:
        m = "none"
    # 4. Guarded read via `not in` early return should NOT be flagged
    if "early_guard_key" not in ev:
        pass
    else:
        _ = ev["early_guard_key"]
    # 5. Ternary expression guard should NOT be flagged
    v = ev["ternary_key"] if "ternary_key" in ev else "default"
    # 6. Genuine unguarded typo read MUST still be flagged
    bad = ev["evnt_id"]
    return f"{t}:{m}:{v}:{bad}"
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("dict-contract-mismatch")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);

    assert_eq!(
        report.findings.len(),
        1,
        "Expected only unguarded typo key 'evnt_id' to be flagged; got: {:?}",
        report.findings
    );
    assert!(report.findings[0].message.contains("'evnt_id'"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_dead_code_dunder_protocols_and_nested_closures() {
    let root = create_temp_workspace("dead_code_dunders_closures");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n[project.scripts]\nrun = \"app.mod:run\"\n",
    )
    .unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("mod.py"),
        r#"def __getattr__(name: str):
    return _lazy_lookup(name)

def __dir__() -> list[str]:
    return ["run"]

def _lazy_lookup(name: str):
    return name

class CustomContainer:
    def __len__(self) -> int:
        return 0

    def __getitem__(self, item: str) -> str:
        return item

    def __setitem__(self, key: str, val: str) -> None:
        pass

    def __contains__(self, item: str) -> bool:
        return False

    def __missing__(self, key: str) -> str:
        return ""

    def __deepcopy__(self, memo: dict):
        return self

    def __getstate__(self) -> dict:
        return {}

    def __setstate__(self, state: dict) -> None:
        pass

    def __class_getitem__(cls, item):
        return cls

def traced(fn):
    def wrapper(*args, **kwargs):
        return fn(*args, **kwargs)
    return wrapper

@traced
def run() -> int:
    def local_step(x: int) -> int:
        return x + 1
    c = CustomContainer()
    return local_step(0)
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();
    let cfg = ResolvedCheckConfig::load_and_resolve(&root, None, Some("dead-code")).unwrap();
    let report = run_checks(&idx, &cfg, None, None);

    assert!(
        report.findings.is_empty(),
        "No dunder methods, module __getattr__/__dir__, decorator wrappers, or called nested helpers should be flagged as dead-code; got: {:?}",
        report.findings
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_unused_parameter_exemptions_and_check_options() {
    let root = create_temp_workspace("unused_param_exemptions");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n[project.scripts]\nrun = \"app.main:main\"\n",
    )
    .unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("main.py"),
        r#"from abc import ABC, abstractmethod
from typing import Protocol, overload

class HandlerProto(Protocol):
    def handle(self, event: dict, context: dict) -> None: ...

class BaseService(ABC):
    @abstractmethod
    def process(self, payload: str, metadata: dict) -> int:
        raise NotImplementedError

    def stub_hook(self, item: str, flags: int) -> None:
        """Override in subclass."""
        pass

class ConcreteResource:
    def __exit__(self, exc_type, exc_val, exc_tb) -> bool:
        return False

def on_sigterm(signum, frame) -> None:
    print("shutting down")

def always_same_constant(x: int, mode: str) -> int:
    return x + len(mode)

def main() -> int:
    r = ConcreteResource()
    r.__exit__(None, None, None)
    r.__exit__(None, None, None)
    on_sigterm(15, None)
    on_sigterm(2, None)
    return always_same_constant(1, "fast") + always_same_constant(2, "fast")
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();

    // Default options: dunders, Protocols, ABCs, stubs, and 2-arg signal handlers are exempt;
    // `always_same_constant`'s `mode` parameter is flagged because check_constant_args=true.
    let cfg_default =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("unused-parameter")).unwrap();
    let report_default = run_checks(&idx, &cfg_default, None, None);
    assert_eq!(
        report_default.findings.len(),
        1,
        "Expected only always_same_constant(mode) to fire by default, got: {:?}",
        report_default.findings
    );
    assert!(report_default.findings[0].message.contains("'mode'"));

    // Disable constant-arg sub-check and enable check_dunders via CLI --check-option
    let cli_opts = vec![
        "unused-parameter.check_constant_args=false".to_string(),
        "unused-parameter.check_dunders=true".to_string(),
    ];
    let cfg_custom = ResolvedCheckConfig::load_and_resolve_with_options(
        &root,
        None,
        Some("unused-parameter"),
        &cli_opts,
    )
    .unwrap();
    let report_custom = run_checks(&idx, &cfg_custom, None, None);
    assert!(
        report_custom
            .findings
            .iter()
            .all(|f| f.scope.ends_with("__exit__")),
        "With check_constant_args=false and check_dunders=true, only __exit__ params should fire; got: {:?}",
        report_custom.findings
    );
    assert_eq!(report_custom.findings.len(), 3);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_uncaught_exception_escape_check_main_and_cli_check_options() {
    let root = create_temp_workspace("uncaught_exc_check_main");
    let pkg = root.join("src").join("app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();

    fs::write(
        pkg.join("cli_script.py"),
        r#"def validate_input(val: str) -> int:
    if not val:
        raise ValueError("empty input")
    return int(val)

if __name__ == "__main__":
    validate_input("")
"#,
    )
    .unwrap();

    let sel = SourceSelectionOptions {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let idx = pyrx::bake_program_index(&sel, Vec::new(), 0).unwrap();

    // Default: uncaught-exception-escape.check_main = false -> 0 findings on `<module>` / `main_guard`
    let cfg_default =
        ResolvedCheckConfig::load_and_resolve(&root, None, Some("uncaught-exception-escape"))
            .unwrap();
    let report_default = run_checks(&idx, &cfg_default, None, None);
    assert!(
        report_default.findings.is_empty(),
        "Default check_main=false should skip <module>/main_guard entry points; got: {:?}",
        report_default.findings
    );

    // Opt in via CLI -O uncaught-exception-escape.check_main=true
    let pyrx_bin = env!("CARGO_BIN_EXE_pyrx");
    let out = std::process::Command::new(pyrx_bin)
        .args([
            "check",
            "--project",
            root.to_str().unwrap(),
            "--checks",
            "uncaught-exception-escape",
            "-O",
            "uncaught-exception-escape.check_main=true",
        ])
        .output()
        .unwrap();
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout_str.contains("[uncaught-exception-escape]"),
        "Opting into uncaught-exception-escape.check_main=true via -O should report finding; stdout: {}, stderr: {}",
        stdout_str,
        String::from_utf8_lossy(&out.stderr)
    );

    // Also test --check-options catalogue output
    let out_opts = std::process::Command::new(pyrx_bin)
        .args(["check", "--check-options"])
        .output()
        .unwrap();
    assert!(out_opts.status.success());
    let opts_stdout = String::from_utf8_lossy(&out_opts.stdout);
    assert!(opts_stdout.contains("unused-parameter.check_dunders"));
    assert!(opts_stdout.contains("uncaught-exception-escape.check_main"));
    assert!(opts_stdout.contains("semantic-clone.min_statements"));

    let _ = fs::remove_dir_all(&root);
}
