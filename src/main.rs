use pyrx::bake_program_index;
use pyrx::check::{
    ALL_CHECKS, BaselineFile, FailOnThreshold, ResolvedCheckConfig, build_baseline_file,
    render_json_report, render_sarif_report, render_text_report, run_checks,
};
use pyrx::diff::compute_semantic_diff;
use pyrx::graph::ProgramIndex;
use pyrx::model::*;
use pyrx::morph::{MorphRulesFile, run_consolidate, run_declarative_rules};
use pyrx::query::{REGISTERED_TOOLS, execute_tool, find_tool_descriptor, format_tool_output};
use pyrx::snapshot::{
    load_snapshot, load_snapshot_info, merge_snapshots, resolve_index_path, save_snapshot,
    write_file_atomically,
};
use pyrx::workspace::SourceSelectionOptions;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run_cli(&args);
    exit(code.code());
}

fn run_cli(args: &[String]) -> ExitCode {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_top_help();
        return if args.is_empty() {
            ExitCode::UsageError
        } else {
            ExitCode::Success
        };
    }
    if args[0] == "--version" || args[0] == "-V" || args[0] == "version" {
        println!("pyRx {}", pyrx::VERSION);
        return ExitCode::Success;
    }

    match args[0].as_str() {
        "megascope" => run_megascope(&args[1..]),
        "check" => run_check_cmd(&args[1..]),
        "morph" => run_morph_cmd(&args[1..]),
        other => {
            eprintln!(
                "error: unknown subcommand '{other}' (expected 'megascope', 'check', or 'morph')"
            );
            ExitCode::UsageError
        }
    }
}

fn print_top_help() {
    println!(
        "pyRx {} — Whole-Program Call-Graph Indexing, Cross-Module Static Analysis & Refactoring for Python\n\n\
         USAGE:\n  \
           pyrx megascope <verb> [options]\n  \
           pyrx check [options]\n  \
           pyrx morph <consolidate|rewrite> [options]\n\n\
         SUBCOMMANDS:\n  \
           megascope   Whole-program call/import/control-flow index & query engine (.pyrx)\n  \
           check       Cross-module static analysis, reachability, and anti-sprawl checks\n  \
           morph       Multi-file CST refactoring & duplicate-consolidation codemods",
        pyrx::VERSION
    );
}

// ---------------------------------------------------------------------------
// `pyrx megascope`
// ---------------------------------------------------------------------------

fn run_megascope(args: &[String]) -> ExitCode {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_megascope_help();
        return if args.is_empty() {
            ExitCode::UsageError
        } else {
            ExitCode::Success
        };
    }

    let verb = args[0].as_str();
    let rest = &args[1..];
    match verb {
        "index" => run_megascope_index(rest),
        "merge" => run_megascope_merge(rest),
        "info" => run_megascope_info(rest),
        "dump" => run_megascope_dump(rest),
        "diff" => run_megascope_diff(rest),
        "batch" => run_megascope_batch(rest),
        "tools" => {
            let tools_list: Vec<Value> = REGISTERED_TOOLS
                .iter()
                .map(|t| {
                    json!({
                        "name": t.cli_name,
                        "canonicalName": t.name,
                        "description": t.description,
                        "recordsKey": t.records_key,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "tools": tools_list })).unwrap()
            );
            ExitCode::Success
        }
        "call" => {
            if rest.is_empty() {
                eprintln!("error: 'pyrx megascope call' requires a tool name");
                return ExitCode::UsageError;
            }
            run_megascope_tool(&rest[0], &rest[1..])
        }
        tool_name => run_megascope_tool(tool_name, rest),
    }
}

fn print_megascope_help() {
    println!(
        "pyRx megascope — Whole-Program Index & Query Engine (.pyrx)\n\n\
         INDEX & REDUCER VERBS:\n  \
           index       Index Python sources into a deterministic .pyrx snapshot\n  \
           merge       Merge per-module or per-target .pyrx shards into a unified snapshot\n  \
           info        Inspect .pyrx snapshot metadata, provenance, and section integrity\n  \
           dump        Stream all indexed call-site contexts and import edges\n  \
           diff        Semantic diff and change-impact analysis between two .pyrx snapshots\n  \
           batch       Process NDJSON tool queries over stdin\n  \
           tools       List all available megascope query verbs\n\n\
         QUERY VERBS:\n  \
           get-callers              Find callers of a function, method, or class\n  \
           get-callees              Find callees of a function, method, or module\n  \
           get-call-paths           Find interprocedural call chains\n  \
           get-call-hierarchy       Inspect MRO/Protocol hierarchy or caller/callee tree\n  \
           get-import-graph         Query cross-module import edges\n  \
           get-import-chains        Find transitive import chains (why A imports B)\n  \
           query-throw-propagation  Trace interprocedural exception propagation\n  \
           query-nearest-catches    Find nearest enclosing try/except handlers\n  \
           query-async-hazards      Find paths from async def to blocking calls\n  \
           query-context-managers   Query active with/async with locks and resources\n  \
           analyze-dead-code        Entry-point reachability across modules & functions\n  \
           analyze-change-impact    Transitive impact of a git diff, file, or symbol"
    );
}

fn run_megascope_index(args: &[String]) -> ExitCode {
    let mut sel = SourceSelectionOptions::default();
    let mut index_out: Option<PathBuf> = None;
    let mut collapse_paths: Vec<String> = Vec::new();
    let mut threads: usize = 0;
    let mut allow_parse_failures = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (flag, inline_val) = split_inline_flag(arg);
        match flag {
            "--source" | "-s" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a path");
                    return ExitCode::UsageError;
                };
                sel.sources.push(PathBuf::from(val));
            }
            "--source-list" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-list requires a file path");
                    return ExitCode::UsageError;
                };
                sel.source_list = Some(PathBuf::from(val));
            }
            "--source-re" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-re requires a regex");
                    return ExitCode::UsageError;
                };
                sel.source_re = Some(val);
            }
            "--skip-paths" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --skip-paths requires a pattern");
                    return ExitCode::UsageError;
                };
                sel.skip_paths.push(val);
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a directory");
                    return ExitCode::UsageError;
                };
                sel.project_root = Some(PathBuf::from(val));
            }
            "--module-name" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --module-name requires a module qualified name");
                    return ExitCode::UsageError;
                };
                sel.module_name_override = Some(val);
            }
            "--index" | "--output" | "-o" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a file path");
                    return ExitCode::UsageError;
                };
                index_out = Some(PathBuf::from(val));
            }
            "--entry-point" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --entry-point requires a symbol name");
                    return ExitCode::UsageError;
                };
                sel.extra_entry_points.push(val);
            }
            "--collapse-paths" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --collapse-paths requires a pattern");
                    return ExitCode::UsageError;
                };
                collapse_paths.push(val);
            }
            "--threads" | "-j" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --threads requires an integer");
                    return ExitCode::UsageError;
                };
                let Ok(n) = val.parse::<usize>() else {
                    eprintln!("error: invalid --threads value '{val}'");
                    return ExitCode::UsageError;
                };
                threads = n;
            }
            "--allow-parse-failures" => {
                allow_parse_failures = true;
            }
            "--force" | "--no-wait" | "--retry-failed" => {}
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope index'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let prog_index = match bake_program_index(&sel, collapse_paths, threads) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
    };

    let target_path = resolve_index_path(index_out.as_deref(), sel.project_root.as_deref());

    let indexed_count = prog_index
        .meta
        .modules
        .iter()
        .filter(|m| m.outcome == ParseOutcome::Indexed)
        .count();
    let failed_count = prog_index.meta.modules.len() - indexed_count;

    if indexed_count == 0 && failed_count > 0 {
        eprintln!("error: no modules parsed cleanly; refusing to publish snapshot");
        return ExitCode::IndexError;
    }

    if let Err(e) = save_snapshot(&target_path, &prog_index) {
        eprintln!("error: {e}");
        return ExitCode::IndexError;
    }

    let summary = json!({
        "status": "ok",
        "index": target_path.to_string_lossy(),
        "bake": prog_index.meta.bake_id,
        "modules": prog_index.meta.modules.len(),
        "indexed": indexed_count,
        "failed": failed_count,
        "entities": prog_index.entities.len(),
        "callEdges": prog_index.calls.len(),
        "importEdges": prog_index.imports.len(),
    });
    println!("{}", serde_json::to_string(&summary).unwrap());

    if failed_count > 0 && !allow_parse_failures {
        ExitCode::IndexError
    } else {
        ExitCode::Success
    }
}

fn run_megascope_merge(args: &[String]) -> ExitCode {
    let mut output_path: Option<PathBuf> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut input_list: Option<PathBuf> = None;
    let mut project_root: Option<String> = None;
    let mut extra_eps: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--output" | "--index" | "-o" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires an output path");
                    return ExitCode::UsageError;
                };
                output_path = Some(PathBuf::from(val));
            }
            "--input" | "-i" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires an input .pyrx path");
                    return ExitCode::UsageError;
                };
                inputs.push(PathBuf::from(val));
            }
            "--input-list" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --input-list requires a file path");
                    return ExitCode::UsageError;
                };
                input_list = Some(PathBuf::from(val));
            }
            "--project-root" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --project-root requires a path");
                    return ExitCode::UsageError;
                };
                project_root = Some(val);
            }
            "--entry-point" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --entry-point requires a symbol");
                    return ExitCode::UsageError;
                };
                extra_eps.push(val);
            }
            "--allow-mismatch" | "--no-wait" => {}
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope merge'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let Some(out_path) = output_path else {
        eprintln!("error: 'megascope merge' requires --output <file.pyrx>");
        return ExitCode::UsageError;
    };

    if let Some(list_file) = input_list {
        let content = match fs::read_to_string(&list_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "error: failed to read --input-list '{}': {e}",
                    list_file.display()
                );
                return ExitCode::UsageError;
            }
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                inputs.push(PathBuf::from(trimmed));
            }
        }
    }

    if inputs.is_empty() {
        eprintln!("error: 'megascope merge' requires at least one --input or --input-list");
        return ExitCode::UsageError;
    }

    let merged = match merge_snapshots(&inputs, project_root, &extra_eps) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::IndexError;
        }
    };

    if let Err(e) = save_snapshot(&out_path, &merged) {
        eprintln!("error: {e}");
        return ExitCode::IndexError;
    }

    let summary = json!({
        "status": "ok",
        "output": out_path.to_string_lossy(),
        "bake": merged.meta.bake_id,
        "inputsMerged": inputs.len(),
        "modules": merged.meta.modules.len(),
        "entities": merged.entities.len(),
        "callEdges": merged.calls.len(),
        "importEdges": merged.imports.len(),
    });
    println!("{}", serde_json::to_string(&summary).unwrap());
    ExitCode::Success
}

fn run_megascope_info(args: &[String]) -> ExitCode {
    let mut index_path: Option<PathBuf> = None;
    let mut project_root: Option<PathBuf> = None;
    let mut show_files = false;
    let mut pretty = false;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--index" | "-i" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a path");
                    return ExitCode::UsageError;
                };
                project_root = Some(PathBuf::from(val));
            }
            "--files" => show_files = true,
            "--pretty" => pretty = true,
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope info'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let resolved = resolve_index_path(index_path.as_deref(), project_root.as_deref());
    let (hdr, meta) = match load_snapshot_info(&resolved) {
        Ok(res) => res,
        Err(e) => {
            let err_json = json!({
                "status": "unavailable",
                "error": e,
            });
            println!("{}", serde_json::to_string(&err_json).unwrap());
            return ExitCode::IndexError;
        }
    };

    let scope = IndexScope::from_meta(&meta, IndexFreshness::Unchecked);
    let mut payload = json!({
        "status": "ok",
        "index": resolved.to_string_lossy(),
        "formatVersion": meta.format_version,
        "pyrexVersion": meta.pyrex_version,
        "freshness": "unchecked",
        "provenance": {
            "bake": meta.bake_id,
            "projectRoot": meta.project_root,
            "configuredEntryPoints": meta.configured_entry_points,
            "collapsePaths": meta.collapse_paths,
        },
        "summary": hdr,
        "indexScope": scope,
    });
    if show_files {
        payload.as_object_mut().unwrap().insert(
            "files".to_string(),
            serde_json::to_value(&meta.modules).unwrap(),
        );
    }

    if pretty {
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    } else {
        println!("{}", serde_json::to_string(&payload).unwrap());
    }
    ExitCode::Success
}

fn run_megascope_dump(args: &[String]) -> ExitCode {
    let mut index_path: Option<PathBuf> = None;
    let mut sel = SourceSelectionOptions::default();
    let mut format = "ndjson".to_string();
    let mut pretty = false;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--index" | "-i" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--source" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source requires a path");
                    return ExitCode::UsageError;
                };
                sel.sources.push(PathBuf::from(val));
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a path");
                    return ExitCode::UsageError;
                };
                sel.project_root = Some(PathBuf::from(val));
            }
            "--format" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --format requires ndjson|json");
                    return ExitCode::UsageError;
                };
                format = val;
            }
            "--pretty" => pretty = true,
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope dump'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let (prog_index, _freshness) = match load_or_bake_index(index_path.as_deref(), &sel) {
        Ok(res) => res,
        Err((msg, code)) => {
            eprintln!("error: {msg}");
            return code;
        }
    };

    if format == "json" {
        let doc = json!({
            "contexts": prog_index.contexts,
            "imports": prog_index.imports,
        });
        if pretty {
            println!("{}", serde_json::to_string_pretty(&doc).unwrap());
        } else {
            println!("{}", serde_json::to_string(&doc).unwrap());
        }
    } else {
        for ctx in &prog_index.contexts {
            println!(
                "{}",
                serde_json::to_string(&json!({ "kind": "call_context", "record": ctx })).unwrap()
            );
        }
        for imp in &prog_index.imports {
            println!(
                "{}",
                serde_json::to_string(&json!({ "kind": "import_edge", "record": imp })).unwrap()
            );
        }
    }
    ExitCode::Success
}

fn run_megascope_diff(args: &[String]) -> ExitCode {
    let mut before_path: Option<PathBuf> = None;
    let mut after_path: Option<PathBuf> = None;
    let mut with_impact = false;
    let mut max_depth = 10usize;
    let mut pretty = false;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--before" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --before requires a .pyrx path");
                    return ExitCode::UsageError;
                };
                before_path = Some(PathBuf::from(val));
            }
            "--after" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --after requires a .pyrx path");
                    return ExitCode::UsageError;
                };
                after_path = Some(PathBuf::from(val));
            }
            "--impact" => with_impact = true,
            "--max-depth" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --max-depth requires an integer");
                    return ExitCode::UsageError;
                };
                max_depth = val.parse().unwrap_or(10);
            }
            "--pretty" => pretty = true,
            "--allow-mismatch" | "--no-context" => {}
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope diff'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let (Some(bp), Some(ap)) = (before_path, after_path) else {
        eprintln!("error: 'megascope diff' requires both --before <A.pyrx> and --after <B.pyrx>");
        return ExitCode::UsageError;
    };

    let before_idx = match load_snapshot(&bp) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("error loading --before: {e}");
            return ExitCode::IndexError;
        }
    };
    let after_idx = match load_snapshot(&ap) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("error loading --after: {e}");
            return ExitCode::IndexError;
        }
    };

    let diff_report = compute_semantic_diff(&before_idx, &after_idx, with_impact, max_depth);
    let scope = IndexScope::from_meta(&after_idx.meta, IndexFreshness::Unchecked);
    let mut payload = serde_json::to_value(&diff_report).unwrap();
    if let Value::Object(ref mut map) = payload {
        map.insert("status".to_string(), json!("ok"));
        map.insert(
            "indexScope".to_string(),
            serde_json::to_value(&scope).unwrap(),
        );
    }

    if pretty {
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    } else {
        println!("{}", serde_json::to_string(&payload).unwrap());
    }
    ExitCode::Success
}

fn run_megascope_batch(args: &[String]) -> ExitCode {
    let mut index_path: Option<PathBuf> = None;
    let mut project_root: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--index" | "-i" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a path");
                    return ExitCode::UsageError;
                };
                project_root = Some(PathBuf::from(val));
            }
            other => {
                eprintln!("error: unknown flag '{other}' for 'megascope batch'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    let resolved = resolve_index_path(index_path.as_deref(), project_root.as_deref());
    let prog_index = match load_snapshot(&resolved) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::IndexError;
        }
    };

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line_res in stdin.lock().lines() {
        let Ok(line) = line_res else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: Result<Value, _> = serde_json::from_str(trimmed);
        let Ok(Value::Object(req)) = parsed else {
            let resp = json!({
                "error": "Invalid JSON object on batch stdin",
                "status": "usage_error",
                "exit": 2,
            });
            let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
            continue;
        };
        let req_id = req.get("id").cloned();
        let Some(tool_name) = req.get("tool").and_then(|v| v.as_str()) else {
            let mut resp = Map::new();
            if let Some(id) = req_id {
                resp.insert("id".to_string(), id);
            }
            resp.insert(
                "error".to_string(),
                json!("Batch request missing 'tool' string field"),
            );
            resp.insert("status".to_string(), json!("usage_error"));
            resp.insert("exit".to_string(), json!(2));
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string(&Value::Object(resp)).unwrap()
            );
            continue;
        };
        let empty_args = Map::new();
        let tool_args = req
            .get("args")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty_args);

        let (result_payload, exit_code) =
            execute_tool(tool_name, tool_args, &prog_index, IndexFreshness::Unchecked);
        let status_str = result_payload
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("ok")
            .to_string();

        let mut resp = Map::new();
        if let Some(id) = req_id {
            resp.insert("id".to_string(), id);
        }
        resp.insert("tool".to_string(), json!(tool_name));
        resp.insert("status".to_string(), json!(status_str));
        resp.insert("exit".to_string(), json!(exit_code.code()));
        resp.insert("result".to_string(), result_payload);
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::to_string(&Value::Object(resp)).unwrap()
        );
    }
    ExitCode::Success
}

fn run_megascope_tool(tool_name: &str, args: &[String]) -> ExitCode {
    let Some(desc) = find_tool_descriptor(tool_name) else {
        eprintln!("error: unknown megascope verb or tool '{tool_name}'");
        return ExitCode::UsageError;
    };

    let mut index_path: Option<PathBuf> = None;
    let mut sel = SourceSelectionOptions::default();
    let mut format = "json".to_string();
    let mut pretty = false;
    let mut tool_args = Map::new();
    let mut patch_file: Option<PathBuf> = None;
    let mut git_base: Option<String> = None;
    let mut git_head: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--index" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--source" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source requires a path");
                    return ExitCode::UsageError;
                };
                sel.sources.push(PathBuf::from(val));
            }
            "--source-list" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-list requires a path");
                    return ExitCode::UsageError;
                };
                sel.source_list = Some(PathBuf::from(val));
            }
            "--source-re" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-re requires a regex");
                    return ExitCode::UsageError;
                };
                sel.source_re = Some(val);
            }
            "--skip-paths" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --skip-paths requires a pattern");
                    return ExitCode::UsageError;
                };
                sel.skip_paths.push(val);
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a path");
                    return ExitCode::UsageError;
                };
                sel.project_root = Some(PathBuf::from(val));
            }
            "--format" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --format requires json|ndjson|tsv");
                    return ExitCode::UsageError;
                };
                format = val;
            }
            "--pretty" => pretty = true,
            "--patch-file" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --patch-file requires a path");
                    return ExitCode::UsageError;
                };
                patch_file = Some(PathBuf::from(val));
            }
            "--git-base" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --git-base requires a git revision");
                    return ExitCode::UsageError;
                };
                git_base = Some(val);
            }
            "--git-head" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --git-head requires a git revision");
                    return ExitCode::UsageError;
                };
                git_head = Some(val);
            }
            "--args" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --args requires a JSON object string");
                    return ExitCode::UsageError;
                };
                let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&val) else {
                    eprintln!("error: --args value must be a valid JSON object");
                    return ExitCode::UsageError;
                };
                for (k, v) in obj {
                    tool_args.insert(k.replace('-', "_"), v);
                }
            }
            other if other.starts_with("--") => {
                let param_key = other.trim_start_matches("--").replace('-', "_");
                if let Some(iv) = inline_val {
                    insert_cli_tool_arg(&mut tool_args, &param_key, parse_scalar_val(&iv));
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    i += 1;
                    insert_cli_tool_arg(&mut tool_args, &param_key, parse_scalar_val(&args[i]));
                } else {
                    insert_cli_tool_arg(&mut tool_args, &param_key, Value::Bool(true));
                }
            }
            other => {
                eprintln!("error: unexpected positional argument '{other}'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    if patch_file.is_some() || git_base.is_some() {
        match load_patch_text(
            patch_file.as_deref(),
            git_base.as_deref(),
            git_head.as_deref(),
            sel.project_root.as_deref(),
        ) {
            Ok(Some(p)) => {
                tool_args.insert("patch".to_string(), Value::String(p));
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::UsageError;
            }
        }
    }

    let (prog_index, freshness) = match load_or_bake_index(index_path.as_deref(), &sel) {
        Ok(res) => res,
        Err((msg, code)) => {
            let err_json = json!({
                "status": if code == ExitCode::IndexError { "unavailable" } else { "usage_error" },
                "error": msg,
            });
            println!("{}", serde_json::to_string(&err_json).unwrap());
            return code;
        }
    };

    let (payload, exit_code) = execute_tool(tool_name, &tool_args, &prog_index, freshness);
    let rendered = format_tool_output(&payload, desc.records_key, &format, pretty);
    print!("{rendered}");
    exit_code
}

fn insert_cli_tool_arg(map: &mut Map<String, Value>, key: &str, val: Value) {
    if let Some(existing) = map.get_mut(key) {
        if let Value::Array(arr) = existing {
            arr.push(val);
        } else {
            let prev = existing.clone();
            *existing = Value::Array(vec![prev, val]);
        }
    } else {
        map.insert(key.to_string(), val);
    }
}

fn parse_scalar_val(s: &str) -> Value {
    if s == "true" {
        return Value::Bool(true);
    }
    if s == "false" {
        return Value::Bool(false);
    }
    if let Ok(n) = s.parse::<i64>() {
        return json!(n);
    }
    Value::String(s.to_string())
}

fn load_or_bake_index(
    index_path: Option<&Path>,
    sel: &SourceSelectionOptions,
) -> Result<(ProgramIndex, IndexFreshness), (String, ExitCode)> {
    // Ephemeral mode: if `--source`, `--source-list`, or `--source-re` is provided without `--index`
    if index_path.is_none()
        && (!sel.sources.is_empty() || sel.source_list.is_some() || sel.source_re.is_some())
    {
        let idx = bake_program_index(sel, Vec::new(), 0).map_err(|e| (e, ExitCode::UsageError))?;
        return Ok((idx, IndexFreshness::Baked));
    }

    let resolved = resolve_index_path(index_path, sel.project_root.as_deref());
    if resolved.is_file() {
        let idx = load_snapshot(&resolved).map_err(|e| (e, ExitCode::IndexError))?;
        return Ok((idx, IndexFreshness::Unchecked));
    }

    Err((
        format!(
            "Index file not found at '{}' (run 'pyrx megascope index' or pass --source)",
            resolved.display()
        ),
        ExitCode::IndexError,
    ))
}

// ---------------------------------------------------------------------------
// `pyrx check`
// ---------------------------------------------------------------------------

fn run_check_cmd(args: &[String]) -> ExitCode {
    let mut sel = SourceSelectionOptions::default();
    let mut index_path: Option<PathBuf> = None;
    let mut list_checks = false;
    let mut list_check_options = false;
    let mut checks_spec: Option<String> = None;
    let mut checks_config: Option<PathBuf> = None;
    let mut check_option_kvs: Vec<String> = Vec::new();
    let mut format = "text".to_string();
    let mut pretty = false;
    let mut output_file: Option<PathBuf> = None;
    let mut cli_fail_on: Option<FailOnThreshold> = None;
    let mut allow_parse_failures = false;
    let mut baseline_path: Option<PathBuf> = None;
    let mut write_baseline_path: Option<PathBuf> = None;
    let mut patch_file: Option<PathBuf> = None;
    let mut git_base: Option<String> = None;
    let mut git_head: Option<String> = None;
    let mut threads: usize = 0;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--help" | "-h" => {
                print_check_help();
                return ExitCode::Success;
            }
            "--list-checks" => list_checks = true,
            "--list-check-options" => list_check_options = true,
            "--checks" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --checks requires a check specification");
                    return ExitCode::UsageError;
                };
                checks_spec = Some(val);
            }
            "--checks-config" | "--config" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a file path");
                    return ExitCode::UsageError;
                };
                checks_config = Some(PathBuf::from(val));
            }
            "--check-option" | "-O" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires <check>.<key>=<value>");
                    return ExitCode::UsageError;
                };
                check_option_kvs.push(val);
            }
            "--check-options" => {
                if let Some(val) = inline_val {
                    check_option_kvs.extend(parse_check_options_list(&val));
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    check_option_kvs.extend(parse_check_options_list(&args[i]));
                } else {
                    list_check_options = true;
                }
            }
            "--index" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a .pyrx file path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--source" | "-s" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source requires a path");
                    return ExitCode::UsageError;
                };
                sel.sources.push(PathBuf::from(val));
            }
            "--source-list" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-list requires a file path");
                    return ExitCode::UsageError;
                };
                sel.source_list = Some(PathBuf::from(val));
            }
            "--source-re" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-re requires a regex");
                    return ExitCode::UsageError;
                };
                sel.source_re = Some(val);
            }
            "--skip-paths" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --skip-paths requires a pattern");
                    return ExitCode::UsageError;
                };
                sel.skip_paths.push(val);
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a directory");
                    return ExitCode::UsageError;
                };
                sel.project_root = Some(PathBuf::from(val));
            }
            "--entry-point" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --entry-point requires a symbol");
                    return ExitCode::UsageError;
                };
                sel.extra_entry_points.push(val);
            }
            "--format" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --format requires text|json|sarif");
                    return ExitCode::UsageError;
                };
                if !matches!(val.as_str(), "text" | "json" | "sarif") {
                    eprintln!("error: invalid --format '{val}' (expected text|json|sarif)");
                    return ExitCode::UsageError;
                }
                format = val;
            }
            "--pretty" => pretty = true,
            "--output" | "-o" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --output requires a file path");
                    return ExitCode::UsageError;
                };
                output_file = Some(PathBuf::from(val));
            }
            "--fail-on" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --fail-on requires note|warning|error|none");
                    return ExitCode::UsageError;
                };
                match FailOnThreshold::parse(&val) {
                    Ok(t) => cli_fail_on = Some(t),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::UsageError;
                    }
                }
            }
            "--allow-parse-failures" => allow_parse_failures = true,
            "--baseline" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --baseline requires a file path");
                    return ExitCode::UsageError;
                };
                baseline_path = Some(PathBuf::from(val));
            }
            "--write-baseline" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --write-baseline requires a file path");
                    return ExitCode::UsageError;
                };
                write_baseline_path = Some(PathBuf::from(val));
            }
            "--patch-file" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --patch-file requires a file path");
                    return ExitCode::UsageError;
                };
                patch_file = Some(PathBuf::from(val));
            }
            "--git-base" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --git-base requires a revision");
                    return ExitCode::UsageError;
                };
                git_base = Some(val);
            }
            "--git-head" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --git-head requires a revision");
                    return ExitCode::UsageError;
                };
                git_head = Some(val);
            }
            "--threads" | "-j" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --threads requires an integer");
                    return ExitCode::UsageError;
                };
                let Ok(n) = val.parse::<usize>() else {
                    eprintln!("error: invalid --threads '{val}'");
                    return ExitCode::UsageError;
                };
                threads = n;
            }
            "-v" | "--verbose" => {}
            other if !other.starts_with('-') => {
                sel.sources.push(PathBuf::from(other));
            }
            other => {
                eprintln!("error: unknown flag '{other}' for 'pyrx check'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    if list_checks {
        for c in ALL_CHECKS {
            println!(
                "{:<32} [{:<12}] default={:<3} severity={:<7} {}",
                c.name,
                c.category,
                if c.default_enabled { "on" } else { "off" },
                c.default_severity.as_str(),
                c.description
            );
        }
        return ExitCode::Success;
    }

    if list_check_options {
        println!("{:<44} {:<10} DESCRIPTION", "OPTION", "DEFAULT");
        println!(
            "{:<44} {:<10} Include <module> and if __name__ == '__main__' entry points",
            "uncaught-exception-escape.check_main", "false",
        );
        println!(
            "{:<44} {:<10} Inspect Python dunder/protocol methods (__exit__, __getitem__, etc.)",
            "unused-parameter.check_dunders", "false",
        );
        println!(
            "{:<44} {:<10} Flag parameters receiving identical literal constants across all callers",
            "unused-parameter.check_constant_args", "true",
        );
        println!(
            "{:<44} {:<10} Minimum statement count for normalized AST structural clone matching",
            "semantic-clone.min_statements", "5",
        );
        println!(
            "{:<44} {:<10} Comma-separated list of heavy top-level modules to flag",
            "heavy-import-leak.heavy_modules", "(builtin)",
        );
        println!(
            "{:<44} {:<10} Comma-separated list of configuration module stems allowed to read env vars",
            "uncentralized-env-access.allowed_modules", "(builtin)",
        );
        return ExitCode::Success;
    }

    let start_dir = sel
        .project_root
        .clone()
        .or_else(|| sel.sources.first().cloned())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut resolved_cfg = match ResolvedCheckConfig::load_and_resolve(
        &start_dir,
        checks_config.as_deref(),
        checks_spec.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
    };

    for kv in &check_option_kvs {
        if let Err(e) = resolved_cfg.apply_check_option_kv(kv) {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
    }

    let baseline_obj: Option<BaselineFile> = if let Some(ref bp) = baseline_path {
        let content = match fs::read_to_string(bp) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to read --baseline '{}': {e}", bp.display());
                return ExitCode::UsageError;
            }
        };
        match serde_json::from_str(&content) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("error: invalid JSON in --baseline '{}': {e}", bp.display());
                return ExitCode::UsageError;
            }
        }
    } else {
        None
    };

    let patch_text = match load_patch_text(
        patch_file.as_deref(),
        git_base.as_deref(),
        git_head.as_deref(),
        sel.project_root.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
    };

    let prog_index = if let Some(ref ip) = index_path {
        match load_snapshot(ip) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::IndexError;
            }
        }
    } else {
        match bake_program_index(&sel, Vec::new(), threads) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::UsageError;
            }
        }
    };

    let report = run_checks(
        &prog_index,
        &resolved_cfg,
        baseline_obj.as_ref(),
        patch_text.as_deref(),
    );

    eprintln!(
        "{} module(s): {} analyzed, {} failed; {} finding(s) ({} suppressed)",
        report.summary.modules,
        report.summary.analyzed,
        report.summary.failed,
        report.summary.findings,
        report.summary.suppressed
    );

    if let Some(wb_path) = write_baseline_path {
        if report.summary.failed > 0 && !allow_parse_failures {
            eprintln!("error: refusing to write baseline because module parse(s) failed");
            return ExitCode::IndexError;
        }
        let bf = build_baseline_file(&report.findings);
        let json_bytes = serde_json::to_vec_pretty(&bf).unwrap();
        if let Err(e) = write_file_atomically(&wb_path, &json_bytes) {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
        return ExitCode::Success;
    }

    let rendered = match format.as_str() {
        "json" => render_json_report(&report, pretty),
        "sarif" => render_sarif_report(&report),
        _ => render_text_report(&report),
    };

    if let Some(out_p) = output_file {
        if let Err(e) = write_file_atomically(&out_p, rendered.as_bytes()) {
            eprintln!("error: failed to write --output '{}': {e}", out_p.display());
            return ExitCode::UsageError;
        }
    } else {
        print!("{rendered}");
    }

    if report.summary.failed > 0 && !allow_parse_failures {
        return ExitCode::IndexError;
    }

    let has_failing_finding = report
        .findings
        .iter()
        .any(|f| resolved_cfg.should_fail_finding(f, cli_fail_on));
    if has_failing_finding {
        ExitCode::FindingsOrEmpty
    } else {
        ExitCode::Success
    }
}

fn parse_check_options_list(raw: &str) -> Vec<String> {
    // Split on commas that start a new `<check>.<key>=` assignment, while preserving commas inside list values
    let mut out: Vec<String> = Vec::new();
    for segment in raw.split(',') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains('=') || out.is_empty() {
            out.push(trimmed.to_string());
        } else if let Some(last) = out.last_mut() {
            last.push(',');
            last.push_str(trimmed);
        }
    }
    out
}

fn print_check_help() {
    println!(
        "pyRx check — Cross-Module Static Analysis & Anti-Sprawl Checks\n\n\
         USAGE:\n  \
           pyrx check [--source <path>... | --index <file.pyrx>] [options]\n\n\
         OPTIONS:\n  \
           --list-checks                        List all available cross-module checks\n  \
           --checks <spec>                      Comma-separated check/group spec (e.g. 'all,-uncentralized-env-access')\n  \
           --checks-config <file>               Path to .pyrx.toml or .pyrx-check.json\n  \
           --check-option, -O <check>.<k>=<v>   Set a per-check option (repeatable, e.g. -O uncaught-exception-escape.check_main=true)\n  \
           --check-options <k1=v1,k2=v2>        Set comma-separated per-check options\n  \
           --format text|json|sarif             Output format (default: text)\n  \
           --output <file>                      Write report to file\n  \
           --fail-on <sev>                      Exit 1 threshold: note|warning|error|none (default: note)\n  \
           --baseline <file>                    Filter findings against a baseline file\n  \
           --write-baseline <file>              Record current findings to a baseline file\n  \
           --patch-file <file>                  Filter findings to lines changed in unified diff\n  \
           --git-base <rev>                     Filter findings to lines changed since git revision"
    );
}

// ---------------------------------------------------------------------------
// `pyrx morph`
// ---------------------------------------------------------------------------

fn run_morph_cmd(args: &[String]) -> ExitCode {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        print_morph_help();
        return if args.is_empty() {
            ExitCode::UsageError
        } else {
            ExitCode::Success
        };
    }

    let mut is_consolidate = false;
    let mut start_idx = 0usize;
    if args[0] == "consolidate" {
        is_consolidate = true;
        start_idx = 1;
    } else if args[0] == "rewrite" {
        start_idx = 1;
    }

    let mut sel = SourceSelectionOptions::default();
    let mut index_path: Option<PathBuf> = None;
    let mut rules_json_path: Option<PathBuf> = None;
    let mut canonical_target: Option<String> = None;
    let mut duplicates: Vec<String> = Vec::new();
    let mut dry_run = false;
    let mut format = "text".to_string();
    let mut pretty = false;
    let mut threads = 0usize;

    let mut i = start_idx;
    while i < args.len() {
        let (flag, inline_val) = split_inline_flag(&args[i]);
        match flag {
            "--consolidate" | "--all" => is_consolidate = true,
            "--canonical" | "--symbol" => {
                is_consolidate = true;
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a symbol name");
                    return ExitCode::UsageError;
                };
                canonical_target = Some(val);
            }
            "--duplicate" => {
                is_consolidate = true;
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --duplicate requires a symbol name");
                    return ExitCode::UsageError;
                };
                duplicates.push(val);
            }
            "--rules-json" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --rules-json requires a file path");
                    return ExitCode::UsageError;
                };
                rules_json_path = Some(PathBuf::from(val));
            }
            "--index" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --index requires a .pyrx path");
                    return ExitCode::UsageError;
                };
                index_path = Some(PathBuf::from(val));
            }
            "--source" | "-s" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source requires a path");
                    return ExitCode::UsageError;
                };
                sel.sources.push(PathBuf::from(val));
            }
            "--source-list" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --source-list requires a path");
                    return ExitCode::UsageError;
                };
                sel.source_list = Some(PathBuf::from(val));
            }
            "--project-root" | "--project" | "--build-path" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: {flag} requires a directory");
                    return ExitCode::UsageError;
                };
                sel.project_root = Some(PathBuf::from(val));
            }
            "--dry-run" => dry_run = true,
            "--format" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --format requires text|json");
                    return ExitCode::UsageError;
                };
                format = val;
            }
            "--pretty" => pretty = true,
            "--threads" | "-j" => {
                let Some(val) = next_val(args, &mut i, inline_val) else {
                    eprintln!("error: --threads requires an integer");
                    return ExitCode::UsageError;
                };
                threads = val.parse().unwrap_or(0);
            }
            other if !other.starts_with('-') => {
                sel.sources.push(PathBuf::from(other));
            }
            other => {
                eprintln!("error: unknown flag '{other}' for 'pyrx morph'");
                return ExitCode::UsageError;
            }
        }
        i += 1;
    }

    if !is_consolidate && rules_json_path.is_none() {
        eprintln!("error: 'pyrx morph' requires either 'consolidate' or '--rules-json <file>'");
        return ExitCode::UsageError;
    }

    let prog_index = if let Some(ref ip) = index_path {
        match load_snapshot(ip) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::IndexError;
            }
        }
    } else {
        match bake_program_index(&sel, Vec::new(), threads) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::UsageError;
            }
        }
    };

    let project_root = sel.project_root.unwrap_or_else(|| {
        let p = PathBuf::from(&prog_index.meta.project_root);
        let abs = PathBuf::from(format!(
            "/{}",
            prog_index.meta.project_root.trim_start_matches('/')
        ));
        if p.is_absolute() || abs.exists() {
            abs
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    });

    let report_res = if is_consolidate {
        run_consolidate(
            &prog_index,
            &project_root,
            canonical_target.as_deref(),
            &duplicates,
            dry_run,
        )
    } else {
        let rpath = rules_json_path.unwrap();
        let content = match fs::read_to_string(&rpath) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "error: failed to read --rules-json '{}': {e}",
                    rpath.display()
                );
                return ExitCode::UsageError;
            }
        };
        let rules_file: MorphRulesFile = match serde_json::from_str(&content) {
            Ok(rf) => rf,
            Err(e) => {
                eprintln!(
                    "error: invalid JSON in --rules-json '{}': {e}",
                    rpath.display()
                );
                return ExitCode::UsageError;
            }
        };
        run_declarative_rules(&prog_index, &project_root, &rules_file.rules, dry_run)
    };

    let report = match report_res {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::UsageError;
        }
    };

    if format == "json" {
        if pretty {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        } else {
            println!("{}", serde_json::to_string(&report).unwrap());
        }
    } else {
        for ch in &report.changes {
            print!("{}", ch.unified_diff);
        }
        eprintln!(
            "morph: {} file(s) {}, {} edit(s)",
            report.files_changed,
            if report.dry_run {
                "would be modified (dry-run)"
            } else {
                "modified"
            },
            report.total_edits
        );
    }

    ExitCode::Success
}

fn print_morph_help() {
    println!(
        "pyRx morph — Multi-File CST Refactoring & Duplicate Consolidation\n\n\
         USAGE:\n  \
           pyrx morph consolidate [--canonical <symbol>] [--dry-run] [--source <path>...]\n  \
           pyrx morph [rewrite] --rules-json <file> [--dry-run] [--source <path>...]\n\n\
         OPTIONS:\n  \
           --canonical <symbol>   Canonical function to consolidate semantic clones into\n  \
           --duplicate <symbol>   Specific duplicate function(s) to collapse\n  \
           --rules-json <file>    Declarative cross-module CST codemod rules JSON\n  \
           --dry-run              Emit unified diffs without modifying files on disk\n  \
           --format text|json     Output format (default: text)"
    );
}

// ---------------------------------------------------------------------------
// Argument & Git Diff Helpers
// ---------------------------------------------------------------------------

fn split_inline_flag(arg: &str) -> (&str, Option<String>) {
    if arg.starts_with("--") {
        if let Some((k, v)) = arg.split_once('=') {
            return (k, Some(v.to_string()));
        }
    }
    (arg, None)
}

fn next_val(args: &[String], i: &mut usize, inline: Option<String>) -> Option<String> {
    if let Some(v) = inline {
        return Some(v);
    }
    if *i + 1 < args.len() {
        *i += 1;
        Some(args[*i].clone())
    } else {
        None
    }
}

fn load_patch_text(
    patch_file: Option<&Path>,
    git_base: Option<&str>,
    git_head: Option<&str>,
    project_root: Option<&Path>,
) -> Result<Option<String>, String> {
    if let Some(pf) = patch_file {
        if pf.as_os_str() == "-" {
            use std::io::Read;
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("Failed to read --patch-file from stdin: {e}"))?;
            return Ok(Some(buf));
        }
        let s = fs::read_to_string(pf)
            .map_err(|e| format!("Failed to read --patch-file '{}': {e}", pf.display()))?;
        return Ok(Some(s));
    }
    if let Some(base) = git_base {
        let mut cmd = Command::new("git");
        if let Some(root) = project_root {
            cmd.arg("-C").arg(root);
        }
        cmd.args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-color",
            "--no-ext-diff",
            base,
        ]);
        if let Some(head) = git_head {
            cmd.arg(head);
        }
        let out = cmd
            .output()
            .map_err(|e| format!("Failed to execute git diff: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git diff failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    Ok(None)
}
