# Changelog

All notable changes to `pyRx` (`pyrx`) will be documented in this file.

## Unreleased

- Remove automatic function consolidation; report semantic similarity as informational notes. The default failure threshold is now `warning`.
- Index f-string and t-string expressions and nested format specs; resolve uv workspace member globs, exclusions, member scripts, and specific package roots.
- Traverse check call/import graphs without hidden depth limits, with compact three-leading/three-trailing paths and full JSON/SARIF evidence or `-vv` text.
- Include affected callers/importers in patch gating; retain explicit `--diff-scope lines` mode.
- Cache raw module facts by content, frontend identity, and entry-point configuration, then relink the graph; support `--no-cache`.
- Recognize pytest fixture and Home Assistant framework entry points with an opt-out.
- Reject stale/incomplete indexes and invalid Python output for explicit codemods.
- Add an executable acceptance corpus and a reproducible static depth profiler.

## [0.1.0] - 2026-10-19

### Added
- **`pyrx megascope` — Whole-Program Graph & Control-Flow Indexer**:
  - Parallel Python workspace indexer built on `ruff_python_parser`, supporting `pyproject.toml`, `src/` layouts, PEP 420 namespace packages, and `SKILL.md` scripts.
  - Deterministic `.pyrx` and `.mgsc` binary snapshot serialization, shard merging (`megascope merge`), metadata inspection (`megascope info`), and structural snapshot diffing (`megascope diff --impact`).
  - All 16 `megascope` query tools with JSON, NDJSON, and TSV output modes and deterministic exit codes (`0`..`4`).
- **`pyrx check` — 16 Cross-Module Static Analysis & Anti-Sprawl Checks**:
  - **Category A (`sprawl`)**: `semantic-clone`, `schema-drift`, `dict-contract-mismatch` (Load-only & guard-aware), `uncentralized-env-access`.
  - **Category B (`reachability`)**: `dead-code` (recognizing `__all__`, `__init__.py` re-exports, Python dunder protocols, nested closures, and `[entry_points]`), `unused-parameter` (exempting dunders, `Protocol`/`ABC`, stubs, and signal handlers), `test-only-production-code`.
  - **Category C (`exceptions`)**: `uncaught-exception-escape`, `broad-catch-swallow`, `exception-contract-divergence`.
  - **Category D (`async`)**: `blocking-in-async`, `lock-order-inversion`, `unowned-resource-return`.
  - **Category E (`imports`)**: `import-side-effects`, `import-cycle`, `heavy-import-leak` (with Python 3.15 PEP 810 `lazy import` support).
  - Per-check configuration options via `.pyrx.toml` (`[check_options]` / `[rules.<check>]`) and CLI (`-O` / `--check-option <check>.<key>=<value>`, `--check-options`).
  - SARIF 2.1.0 output with line-shift-resilient `pyrxFingerprint/v1` hashes, baseline ratcheting (`--baseline`, `--write-baseline`), and diff-scoped gating (`--patch-file`, `--git-base`).
- **`pyrx morph` — Multi-File CST Refactoring & Consolidation**:
  - Cross-module duplicate function consolidation (`pyrx morph consolidate --canonical <symbol>`) and declarative CST codemods conditioned on `megascope` control-flow facts (`pyrx morph rewrite --rules-json`).
