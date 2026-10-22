use pyrx::{
    bake_program_index, bake_program_index_with_stats,
    check::{CheckSeverity, ResolvedCheckConfig, run_checks},
    graph::ProgramIndex,
    workspace::SourceSelectionOptions,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn workspace(files: &[(&str, &str)]) -> TempDir {
    let root = tempfile::tempdir().unwrap();
    for (file, contents) in files {
        let path = root.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    root
}
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}
fn options(root: &Path) -> SourceSelectionOptions {
    SourceSelectionOptions {
        project_root: Some(root.into()),
        no_cache: true,
        ..Default::default()
    }
}
fn index(root: &Path) -> ProgramIndex {
    bake_program_index(&options(root), vec![], 1).unwrap()
}
fn checks(index: &ProgramIndex, names: &str) -> pyrx::check::CheckRunReport {
    let mut config = ResolvedCheckConfig::default();
    config.apply_spec_tokens(names.split(',')).unwrap();
    run_checks(index, &config, None, None)
}
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pyrx"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn similarity_is_informational_and_cannot_rewrite_code() {
    let root = workspace(&[
        (
            "common.py",
            "def one(x):\n    a = x + 1\n    b = a * 2\n    c = b - 1\n    d = c / 2\n    return d\n",
        ),
        (
            "other.py",
            "def two(x):\n    a = x + 2\n    b = a * 2\n    c = b - 1\n    d = c / 2\n    return d\n",
        ),
    ]);
    let before = fs::read(root.path().join("other.py")).unwrap();
    let result = cli(
        root.path(),
        &[
            "check",
            "--project",
            ".",
            "--checks",
            "semantic-clone",
            "--format",
            "json",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["findings"][0]["severity"], "note");
    assert!(
        report["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not imply equivalent behavior")
    );
    assert_eq!(
        cli(
            root.path(),
            &["check", "--checks", "semantic-clone", "--fail-on", "note"]
        )
        .status
        .code(),
        Some(1)
    );
    for args in [
        vec!["morph", "consolidate", "--canonical", "common.one"],
        vec!["morph", "--consolidate"],
        vec!["morph", "--canonical", "common.one"],
        vec!["morph", "--all"],
    ] {
        assert_eq!(cli(root.path(), &args).status.code(), Some(2));
        assert_eq!(fs::read(root.path().join("other.py")).unwrap(), before);
    }
}

#[test]
fn interpolated_expressions_and_nested_format_specs_are_indexed() {
    let root = workspace(&[(
        "app.py",
        "import time\n\ndef formatted(value, width):\n    return f'{value:{width}}'\n\ndef templated(value, width):\n    return t'{value:{width}}'\n\nasync def serve():\n    return f'{time.sleep(1):{len(str(3))}}'\n",
    )]);
    let idx = index(root.path());
    let report = checks(&idx, "unused-parameter,blocking-in-async");
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.check == "unused-parameter")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.check == "blocking-in-async")
    );
    for callee in ["time.sleep", "len", "str"] {
        assert!(idx.calls.iter().any(|e| e.callee == callee), "{callee}");
    }
    let library = index(&fixture("library"));
    assert!(checks(&library, "unused-parameter").findings.is_empty());
}

#[test]
fn uv_members_use_specific_package_roots_and_discover_member_scripts() {
    let root = fixture("uv_workspace");
    let idx = index(&root);
    let entity = &idx.entities[*idx.entity_by_qname.get("demo.api.main").unwrap()];
    assert!(entity.is_entry_point);
    assert!(
        !idx.meta
            .configured_entry_points
            .contains(&"bad:main".to_string())
    );
    assert!(
        !idx.entities
            .iter()
            .any(|e| e.qualified_name == "packages.core.src.demo.api.main")
    );
}

#[test]
fn deep_recursive_paths_are_analyzed_but_compactly_displayed() {
    let mut source = String::from("import time\nasync def serve():\n    f0()\n");
    for i in 0..60 {
        source += &format!(
            "def f{i}():\n    {}\n",
            if i == 59 {
                "time.sleep(1)\n    f0()".into()
            } else {
                format!("f{}()", i + 1)
            }
        );
    }
    let root = workspace(&[("app.py", &source)]);
    let idx = index(root.path());
    let report = checks(&idx, "blocking-in-async");
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.evidence.len(), 61);
    assert_eq!(finding.full_path().len(), 62);
    assert!(finding.message.contains("app.serve -> app.f0 -> app.f1 -> ... (56 frames omitted) -> app.f58 -> app.f59 -> time.sleep"));
    assert!(!finding.message.contains("app.f30"));
    let verbose = pyrx::check::render_text_report_verbose(&report, 2);
    assert!(verbose.contains("app.f30"));
    assert!(!verbose.contains("frames omitted"));
    let sarif: serde_json::Value =
        serde_json::from_str(&pyrx::check::render_sarif_report(&report)).unwrap();
    assert_eq!(
        sarif["runs"][0]["results"][0]["codeFlows"][0]["threadFlows"][0]["locations"]
            .as_array()
            .unwrap()
            .len(),
        61
    );
    let plain = cli(root.path(), &["check", "--checks", "blocking-in-async"]);
    let verbose = cli(
        root.path(),
        &["check", "--checks", "blocking-in-async", "-vv"],
    );
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("app.f30"));
    assert!(String::from_utf8_lossy(&verbose.stdout).contains("app.f30"));
}

#[test]
fn exception_paths_reach_every_boundary_and_respect_catches() {
    let root = workspace(&[
        (
            "pyproject.toml",
            "[project.scripts]\na = 'app:a'\nb = 'app:b'\nc = 'app:c'\n",
        ),
        (
            "app.py",
            "def a():\n    bad()\ndef b():\n    bad()\ndef c():\n    try:\n        bad()\n    except ValueError:\n        pass\ndef bad():\n    raise ValueError('x')\n",
        ),
    ]);
    let report = checks(&index(root.path()), "uncaught-exception-escape");
    let scopes: Vec<_> = report.findings.iter().map(|f| f.scope.as_str()).collect();
    assert_eq!(scopes, ["app.a", "app.b"]);
    assert_eq!(
        checks(&index(&fixture("cli")), "uncaught-exception-escape")
            .findings
            .len(),
        1
    );
}

#[test]
fn offloaded_work_is_not_a_synchronous_blocking_path() {
    let root = workspace(&[(
        "app.py",
        "import asyncio\nimport time\ndef work():\n    time.sleep(1)\nasync def serve():\n    await asyncio.to_thread(work)\n",
    )]);
    assert!(
        checks(&index(root.path()), "blocking-in-async")
            .findings
            .is_empty()
    );
}

#[test]
fn patch_gate_keeps_cross_module_causes_and_supports_explicit_line_mode() {
    let idx = index(&fixture("async_service"));
    let patch = "diff --git a/worker.py b/worker.py\n--- a/worker.py\n+++ b/worker.py\n@@ -3,2 +3,2 @@\n def work():\n-    pass\n+    time.sleep(1)\n";
    let mut cfg = ResolvedCheckConfig::default();
    cfg.apply_spec_tokens(["blocking-in-async"]).unwrap();
    let report = run_checks(&idx, &cfg, None, Some(patch));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].file, "api.py");
    cfg.diff_lines_only = true;
    assert!(
        run_checks(&idx, &cfg, None, Some(patch))
            .findings
            .is_empty()
    );
    let unrelated = "diff --git a/unrelated.py b/unrelated.py\n--- a/unrelated.py\n+++ b/unrelated.py\n@@ -1 +1 @@\n-x = 1\n+x = 2\n";
    cfg.diff_lines_only = false;
    assert!(
        run_checks(&idx, &cfg, None, Some(unrelated))
            .findings
            .is_empty()
    );
}

#[test]
fn framework_adapters_are_narrow_and_can_be_disabled() {
    let idx = index(&fixture("framework"));
    for qname in [
        "custom_components.demo.async_setup_entry",
        "custom_components.demo.sensor.DemoSensor",
        "custom_components.demo.sensor.DemoSensor.native_value",
        "plugin.connection",
    ] {
        assert!(
            idx.entities[*idx.entity_by_qname.get(qname).unwrap()].is_entry_point,
            "{qname}"
        );
    }
    assert!(
        !idx.entities[*idx.entity_by_qname.get("plugin.async_setup_entry").unwrap()].is_entry_point
    );
    let root = workspace(&[
        (".pyrx.toml", "framework_entry_points = false\n"),
        (
            "plugin.py",
            "import pytest\n@pytest.fixture\ndef connection():\n    return 42\n",
        ),
    ]);
    assert!(
        !index(root.path())
            .entities
            .iter()
            .find(|e| e.name == "connection")
            .unwrap()
            .is_entry_point
    );
}

fn comparable(idx: &ProgramIndex) -> Vec<u8> {
    pyrx::snapshot::serialize_snapshot(idx).unwrap()
}

#[test]
fn cache_matches_cold_analysis_across_edits_configuration_corruption_and_deletions() {
    let root = workspace(&[
        (
            "a.py",
            "from b import work\nasync def serve():\n    work()\n",
        ),
        ("b.py", "def work():\n    pass\n"),
    ]);
    let mut opts = options(root.path());
    opts.no_cache = false;
    let (_, first) = bake_program_index_with_stats(&opts, vec![], 1).unwrap();
    assert_eq!(first.parsed, 2);
    let (warm, second) = bake_program_index_with_stats(&opts, vec![], 4).unwrap();
    assert_eq!(second.reused, 2);
    assert_eq!(second.parsed, 0);
    assert_eq!(comparable(&warm), comparable(&index(root.path())));
    fs::write(
        root.path().join("b.py"),
        "import time\ndef work():\n    time.sleep(1)\n",
    )
    .unwrap();
    let (edited, stats) = bake_program_index_with_stats(&opts, vec![], 4).unwrap();
    assert_eq!((stats.reused, stats.parsed), (1, 1));
    assert_eq!(comparable(&edited), comparable(&index(root.path())));
    assert_eq!(checks(&edited, "blocking-in-async").findings.len(), 1);
    fs::write(
        root.path().join(".pyrx.toml"),
        "entry_points = ['b:work']\n",
    )
    .unwrap();
    let (configured, stats) = bake_program_index_with_stats(&opts, vec![], 1).unwrap();
    assert_eq!(stats.parsed, 2);
    assert!(configured.entities[*configured.entity_by_qname.get("b.work").unwrap()].is_entry_point);
    for entry in fs::read_dir(root.path().join(".pyrx/cache")).unwrap() {
        fs::write(entry.unwrap().path(), "broken").unwrap();
    }
    let (recovered, stats) = bake_program_index_with_stats(&opts, vec![], 4).unwrap();
    assert_eq!(stats.parsed, 2);
    assert_eq!(comparable(&recovered), comparable(&index(root.path())));
    fs::remove_file(root.path().join("b.py")).unwrap();
    let (removed, _) = bake_program_index_with_stats(&opts, vec![], 1).unwrap();
    assert!(!removed.entity_by_qname.contains_key("b.work"));
    assert_eq!(comparable(&removed), comparable(&index(root.path())));
}

#[test]
fn no_cache_does_not_write_into_source_tree() {
    let root = workspace(&[("app.py", "x = 1\n")]);
    index(root.path());
    assert!(!root.path().join(".pyrx").exists());
}

#[test]
fn declarative_rewrites_refuse_stale_sources_and_invalid_output_before_writing() {
    use pyrx::morph::{MorphRule, run_declarative_rules};
    let root = workspace(&[
        ("a.py", "def f():\n    print('a')\n"),
        ("b.py", "def f():\n    print('b')\n"),
    ]);
    let idx = index(root.path());
    let rule: MorphRule = serde_json::from_value(serde_json::json!({"id":"invalid","match_callee":"print","action":"template","template":")"})).unwrap();
    let before = [
        fs::read(root.path().join("a.py")).unwrap(),
        fs::read(root.path().join("b.py")).unwrap(),
    ];
    assert!(
        run_declarative_rules(&idx, root.path(), std::slice::from_ref(&rule), false)
            .unwrap_err()
            .contains("invalid Python")
    );
    assert_eq!(before[0], fs::read(root.path().join("a.py")).unwrap());
    assert_eq!(before[1], fs::read(root.path().join("b.py")).unwrap());
    fs::write(
        root.path().join("b.py"),
        "# changed\ndef f():\n    print('b')\n",
    )
    .unwrap();
    assert!(
        run_declarative_rules(&idx, root.path(), &[rule], false)
            .unwrap_err()
            .contains("Stale index")
    );
    assert_eq!(before[0], fs::read(root.path().join("a.py")).unwrap());
}

#[test]
fn default_note_threshold_can_be_explicitly_overridden() {
    let root = workspace(&[("a.py", "def unused(x):\n    return 1\n")]);
    let report = checks(&index(root.path()), "unused-parameter");
    assert_eq!(report.findings[0].severity, CheckSeverity::Note);
    let cfg = ResolvedCheckConfig::default();
    assert!(!cfg.should_fail_finding(&report.findings[0], None));
    assert!(cfg.should_fail_finding(
        &report.findings[0],
        Some(pyrx::check::FailOnThreshold::Note)
    ));
}

#[test]
fn explicit_async_offload_preserves_executable_behavior_and_nested_awaits() {
    use pyrx::morph::{MorphRule, run_declarative_rules};
    let root = workspace(&[(
        "app.py",
        "#!/usr/bin/env python3\n# coding: utf-8\n# import asyncio (a comment is not an import)\ndef blocking(value):\n    return value + 1\nasync def passthrough(value):\n    return value\nasync def run():\n    return await passthrough(blocking(2))\n",
    )]);
    let execute = || {
        Command::new("python3")
            .current_dir(root.path())
            .args([
                "-B",
                "-c",
                "import app, asyncio; print(asyncio.run(app.run()))",
            ])
            .output()
            .unwrap()
    };
    let before = execute();
    assert!(before.status.success());
    assert_eq!(before.stdout, b"3\n");
    let idx = index(root.path());
    let inner = idx
        .calls
        .iter()
        .find(|e| e.caller == "app.run" && e.callee == "app.blocking")
        .unwrap();
    assert!(!inner.is_awaited);
    let rule: MorphRule = serde_json::from_value(serde_json::json!({"id":"offload", "match_callee":"app.blocking", "action":"wrap_asyncio_to_thread", "when_caller_execution_context":"async"})).unwrap();
    run_declarative_rules(&idx, root.path(), &[rule], false).unwrap();
    let after = execute();
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(after.stdout, before.stdout);
    assert!(
        fs::read_to_string(root.path().join("app.py"))
            .unwrap()
            .starts_with("#!/usr/bin/env python3\n# coding: utf-8\n")
    );
}

#[test]
fn deep_import_and_exception_paths_have_no_hidden_cutoff() {
    let mut source = String::from("import subprocess\n");
    for i in 0..60 {
        source += &format!(
            "def f{i}():\n    {}\n",
            if i == 59 {
                "subprocess.run(['true'])\n    raise ValueError('x')".into()
            } else {
                format!("f{}()", i + 1)
            }
        );
    }
    source += "f0()\n";
    let root = workspace(&[
        ("app.py", &source),
        (".pyrx.toml", "entry_points = ['app:f0']\n"),
    ]);
    let report = checks(
        &index(root.path()),
        "import-side-effects,uncaught-exception-escape",
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.check == "import-side-effects" && f.evidence.len() == 61)
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.check == "uncaught-exception-escape" && f.evidence.len() == 59)
    );
}

#[test]
fn deep_lock_order_and_import_chains_are_not_cut_off() {
    let mut source = String::from(
        "import threading\nfirst = threading.Lock()\nsecond = threading.Lock()\ndef left():\n    with first:\n        f0()\ndef right():\n    with second:\n        with first:\n            pass\n",
    );
    for i in 0..20 {
        source += &format!(
            "def f{i}():\n    {}\n",
            if i == 19 {
                "with second:\n        pass".into()
            } else {
                format!("f{}()", i + 1)
            }
        );
    }
    let root = workspace(&[("app.py", &source)]);
    assert!(
        checks(&index(root.path()), "lock-order-inversion")
            .findings
            .iter()
            .any(|f| f.kind == "lock_acquisition_cycle")
    );
    for i in 0..20 {
        fs::write(
            root.path().join(format!("m{i}.py")),
            if i == 19 {
                "import pandas\n".to_string()
            } else {
                format!("import m{}\n", i + 1)
            },
        )
        .unwrap();
    }
    let report = checks(&index(root.path()), "heavy-import-leak");
    let finding = report
        .findings
        .iter()
        .find(|f| f.scope == "m0.<module>")
        .unwrap();
    assert_eq!(finding.evidence.len(), 20);
    assert!(finding.message.contains("frames omitted"));
}

#[test]
fn exception_contract_summaries_respect_specific_catches() {
    let root = workspace(&[(
        "app.py",
        "from typing import Protocol\nclass Runner(Protocol):\n    def run(self): ...\nclass Catching(Runner):\n    def run(self):\n        try:\n            bad()\n        except ValueError:\n            return None\nclass Returning(Runner):\n    def run(self):\n        return None\ndef bad():\n    raise ValueError('bad')\n",
    )]);
    assert!(
        checks(&index(root.path()), "exception-contract-divergence")
            .findings
            .is_empty()
    );
}
