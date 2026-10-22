# Analysis behavior

## Similarity is informational

`semantic-clone` reports normalized structural similarity at `note` severity. It does not prove behavioral equivalence and does not generate a fix. Automatic consolidation is unsupported. Notes remain visible in text, JSON, and SARIF but do not fail the default `warning` threshold; `--fail-on note` explicitly includes them.

## Traversal and diagnostic paths

Check traversals have no call-depth cutoff. Visited sets and fixed-point propagation terminate cycles, including mutually recursive functions. This follows the finite indexed graph; it does not unroll runtime recursion or prove that a runtime call is possible. Interactive `megascope` queries and snapshot-diff impact retain their separate query depth/paging controls.

Diagnostic messages display at most three leading and three trailing frames, separated by an explicit omitted-frame count. `pyrx check -v` adds source locations and graph-edge confidence; `-vv` displays the entire path. JSON findings contain full `evidence` steps (caller, callee, file, line, column, confidence). SARIF uses `codeFlows` for the same evidence. Display changes do not affect baseline fingerprints.

`analysis` in JSON/SARIF and `-v` text report internal, external/unresolved, and uncertain call-edge counts. These are graph coverage statistics, not a precision score. Dynamic dispatch, reflection, external implementations, and unknown framework registration can hide behavior. Reachability means reachability from known entry points. Configure `entry_points` for public APIs and plugin hooks the tool cannot discover.

## Patch gating

`--patch-file` and `--git-base` default to `--diff-scope impact`: keep findings on changed lines, along changed evidence paths, or associated with affected callers and runtime importers. Thus adding blocking I/O to a helper can report a problem at its unchanged async caller. `--diff-scope lines` explicitly requests the older location-only behavior.

Impact is computed from the current graph. Deleted call edges and removed definitions cannot always be reconstructed from it. For removal-heavy changes or changed framework registration, run an unrestricted check with a committed baseline to identify newly introduced findings. Patch impact is a relevance filter, not a before/after finding comparison. Baseline filtering happens before patch filtering.

## Workspace and framework entry points

An explicit `--project` root is authoritative and does not inherit a containing repository's package roots. uv workspace `members` and `exclude` glob patterns discover member roots and their project scripts; the most specific matching package root determines module identity. Member exclusions control workspace metadata, not the independent source selection filter; use `--skip-paths` to exclude files from analysis.

Framework adapters recognize `pytest.fixture` / `pytest_asyncio.fixture`, Home Assistant lifecycle hooks in integrations with `custom_components/<domain>/manifest.json`, websocket command registration, and classes derived from Home Assistant Entity/ConfigFlow/OptionsFlow bases. Public methods of those framework classes are treated as entry points. These adapters identify external callers without importing or executing Python.

Set `framework_entry_points = false` in `.pyrx.toml` or `[tool.pyrx]` to disable these adapters. Explicit `entry_points`, package exports, and existing CLI/route/test discovery remain available. Additional frameworks require explicit entry-point configuration.

## Incremental indexing

Indexing and checking reuse raw per-module facts under `.pyrx/cache`. Keys include source content, module identity, parser/frontend source and dependency identity, and entry-point configuration. Entries have checksums; corrupt or incompatible entries are reparsed. Cross-module resolution, framework adapters, and export propagation always run again, so a changed producer can affect unchanged consumers. Deleted source files are excluded by fresh source discovery.

`--no-cache` forces parsing and avoids cache writes. `pyrx check -v` reports reused/parsed module counts on stderr; normal reports and snapshots are independent of cache state and thread count. Read-only caches are optional and do not prevent analysis. Cache files are disposable. A removed source can leave an unused cache file; deleting `.pyrx/cache` removes it.

## Explicit codemods

`pyrx morph rewrite --rules-json rules.json --dry-run` previews user-specified transformations. Before producing edits, the engine rejects failed/partial modules and source hashes that differ from the index. It parses all proposed output before writing and rechecks inspected source files immediately before applying the plan. `wrap_asyncio_to_thread` requires an async caller; it cannot silently turn synchronous helpers into coroutine-returning functions.

Rules remain responsible for intended semantic changes. Writes are atomic per file, not a crash-atomic transaction across the repository; use version control to review and recover an interrupted apply.

## Validation and profiling

`cargo test --all-targets` exercises CLI, async-service, library, uv-workspace, and framework fixtures under `tests/fixtures`, plus recursive 60-helper chains, compact/full evidence, source freshness, invalid-output rejection, and cached/cold equivalence across edits, configuration changes, corruption, deletion, and thread counts.

Use `cargo run --release --example depth_profile -- /path/to/project` to measure shortest static call depths from non-test entry points and async functions. Pass `/project/root::/selected/source/directory` to profile one package while retaining its import root. The profiler skips deferred callbacks, writes no cache, and reports failed modules alongside depth distributions and the deepest blocking witness. It measures indexed static paths, not runtime stack depths.
