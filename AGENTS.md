# `pyRx` — Agent & Contributor Guide

## Overview
`pyRx` (`pyrx`) is the Python counterpart to `vycor-cpp`: a deterministic whole-program call-graph indexer, cross-module static analyzer, and multi-file CST refactoring engine built in Rust on top of `ruff_python_parser`.

## Repository Layout
- `../megascope` (`megascope` crate, re-exported by `src/lib.rs`) — Language-agnostic whole-program core shared with `vycor-cpp`:
  - `megascope::model` — Core data model (`EntityNode`, `CallEdge`, `CallSiteContext`, `ImportEdge`, `ClassSchema`, `EnvAccess`, `ChannelSite`, `ModuleShard`, `IndexScope`, `ResultStatus`, `ExitCode`, `StringTable`).
  - `megascope::graph` — Cross-module `ProgramIndex` builder (`from_shards`) performing diamond-dependency deduplication, import alias resolution, C3/BFS MRO, structural `Protocol` & virtual override linking, `super()` resolution, `FunctionPointerReturn` deferred joins, and `--collapse-paths`.
  - `megascope::snapshot` — Deterministic `.pyrx` binary format (`PYRX\x00\x01\x00\x00` magic, `xxh3_64` section checksums, `memmap2` read, atomic write, and `merge_snapshots`).
  - `megascope::diff` — Semantic diff between `.pyrx` snapshots, unified diff parser, and reverse-BFS change impact analysis.
  - `megascope::query` — All 20 `megascope` query tools, identity disambiguation (`not_found` + `didYouMean`, `ambiguous` exit code 4), paging contract, and `json`/`ndjson`/`tsv` output formatting.
- `src/cache.rs` — Content- and frontend-version-validated raw module cache; always relink after reuse.
- `src/frameworks.rs` — Narrow, disableable pytest and Home Assistant entry-point adapters.
- `src/workspace.rs` — Python workspace & module discovery (`pyproject.toml`, `.pyrx.toml`, `src/` layout, `__init__.py` packages, `SKILL.md` scripts, relative import resolution, `requires-python` inspection).
- `src/indexer.rs` — Per-module Python AST & control-flow context indexer (`try`/`except`/`finally`, `with`/`async with`, conditional guards, exception raises, resource allocations, dict shapes, env accesses, Python 3.15 PEP 810 `lazy import`, and alpha-renaming AST/CFG skeleton canonicalizer) emitting `megascope::model::ModuleShard`.
- `src/check/` — `pyrx check` engine, `pyrxFingerprint/v1` stable hashes, inline `# pyrx: ignore[...]` / `# pyrex: ignore[...]` suppressions, `.pyrx.toml` per-check severity & CI exit-code configurability (`fail_on`, `fail`), baselines, diff gating, SARIF 2.1.0 / JSON / text reporters, and all 16 checks across Categories A–E (`sprawl.rs`, `reachability.rs`, `exceptions.rs`, `async_conc.rs`, `imports.rs`).
- `docs/checks/` — Detailed user-facing documentation for all 16 `pyRx` checks (`docs/checks/README.md` and `docs/checks/<check>.md`).
- `src/morph/mod.rs` — Lossless byte-span CST rewriter (explicit declarative `--rules-json` codemods; no automatic consolidation).
- `buck2/pyrex.bzl` & `buck2/pyrex.bxl` — 3-tier Buck2 map-reduce indexing (`PyrexIndexTSet`), check, and query rules plus zero-config BXL scripts.

## Build & Test Commands
```bash
cargo build --release
cargo test
```

## Key Invariants
1. **Byte-for-Byte Determinism**: `.pyrx` snapshots and check/query outputs must be bit-identical regardless of thread count (`--threads 1` vs `--threads N`) or shard merge order. Always use `BTreeMap`/`BTreeSet` or explicit deterministic sorting before serialization.
2. **Exit Codes**:
   - `0`: Clean check run or non-empty query result
   - `1`: Check findings reported (matching `--fail-on` and not configured with `fail = false`), or empty query result / `not_found`
   - `2`: CLI usage or configuration error
   - `3`: Index error, corrupted `.pyrx` checksum, or parse failure
   - `4`: Ambiguous symbol identity
3. **Response Envelope**: Every `megascope` tool JSON/NDJSON payload stamps `status` and `indexScope`.

## Ephemeral Work Artifacts Policy (No Plans or Momentary Notes in VCS)

Temporary development artifacts must stay temporary or go into **GitHub Issues**, never into version control:
- **Never commit** momentary design notes (`docs/design-*.md`), task/work-package plans (`docs/plans/**`, `docs/tasks/**`, `docs/handoffs/**`), architecture/CLI reviews (`docs/*-review.md`), investigation/diagnosis logs, or roadmap/TODO scratch files (`ROADMAP.md`, `CHARTER.md`, `docs/issues/**`).
- **Keep `docs/` strictly for durable, user- or contributor-facing reference documentation** that describes the current system as built (e.g., `docs/checks/**`).
- **Track planned work, follow-ups, design proposals, and open questions in GitHub Issues** (`gh issue create` / `gh issue edit`).
- **Use untracked scratch directories** (`scratch/` or `/tmp/`) for temporary scripts, benchmark logs, and working notes during a session.

## Analysis behavior

Similarity diagnostics are informational and never authorize rewriting. All check traversals are cycle-safe without a depth cutoff; compact display uses three leading and three trailing frames, while JSON evidence and `-vv` preserve full paths. See `docs/analysis.md`. Run `cargo test --all-targets`, `cargo fmt --all -- --check`, and `cargo clippy --all-targets -- -D warnings` after changes.
