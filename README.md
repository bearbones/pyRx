# `pyRx` — Whole-Program Call-Graph Indexing, Cross-Module Static Analysis, and Anti-Sprawl Refactoring for Python

`pyRx` (`pyrx`) is a fast, deterministic whole-program static analysis and refactoring engine for Python, built in Rust on top of `ruff_python_parser`. It is the Python counterpart to `vycor-cpp`, sharing its binary snapshot design, exit-code contract, `status` + `indexScope` provenance envelope, and SARIF 2.1.0 `pyrxFingerprint/v1` baseline workflow.

While single-file linters (`ruff`) and type checkers (`pyright`, `mypy`) operate one file or type signature at a time, `pyRx` builds a whole-repository **Call Graph**, **Import Graph**, **Class Hierarchy (MRO + structural `Protocol`s)**, and **Interprocedural Control-Flow Context Index** (`.pyrx`) to answer cross-module queries, detect LLM-induced codebase sprawl, and perform multi-file CST codemods.

---

## Three Pillars

### 1. `pyrx megascope` — Whole-Program Graph & Control-Flow Query Engine

Indexes Python workspaces (`pyproject.toml`, `src/` layouts, `__init__.py` packages, `SKILL.md` scripts) into deterministic, memory-mapped `.pyrx` binary snapshots with per-section `xxh3` checksums and map-reduce shard merging.

```bash
# Index a workspace into a .pyrx snapshot
pyrx megascope index --project . --output .pyrx/index.pyrx

# Merge per-target .pyrx shards (with diamond-dependency deduplication)
pyrx megascope merge --input shard_a.pyrx --input shard_b.pyrx --output merged.pyrx

# Inspect snapshot metadata or diff two snapshots with blast-radius impact analysis
pyrx megascope info --index .pyrx/index.pyrx --pretty
pyrx megascope diff --before old.pyrx --after new.pyrx --impact --pretty
```

#### 16 Query Tools (`--format json|ndjson|tsv`)
| Tool | Description |
| :--- | :--- |
| `get-callers` | Direct and MRO/Protocol polymorphic callers of a symbol |
| `get-callees` | Direct and dynamic-dispatch callees of a function, method, or `<module>` |
| `get-call-paths` | Shortest interprocedural call paths between functions or from entry points |
| `get-call-hierarchy` | Class MRO/Protocol hierarchy, overrides, or multi-hop call tree |
| `get-import-graph` | Cross-module import edges (runtime vs `if TYPE_CHECKING:` vs lazy) |
| `get-import-chains` | Transitive import chains explaining why module A imports package B |
| `query-throw-propagation` | Interprocedural exception propagation & uncaught escape paths |
| `query-nearest-catches` | Nearest enclosing `try`/`except` handlers along reverse caller chains |
| `query-async-hazards` | Paths from `async def` coroutines to synchronous blocking calls or `threading.Lock` |
| `query-context-managers` | Active `with`/`async with` scopes and interprocedural lock acquisition order |
| `analyze-dead-code` | Whole-program reachability analysis (including mutually-recursive dead subgraphs) |
| `analyze-change-impact` | Reverse-BFS blast radius from modified symbols, files, or unified diffs |
| `search-functions` | Search indexed Python entities by substring or glob pattern |
| `lookup-function` | Detailed entity metadata, signature, decorators, MRO, and raise sites |
| `list-entry-points` | Discovered `[project.scripts]`, pytest, FastAPI/Click/Typer, `__all__`, `__init__.py`, and `SKILL.md` entry points |
| `graph-summary` | Whole-program module, entity, call-edge, and import-edge statistics |

---

### 2. `pyrx check` — Cross-Module Static Analysis & Anti-Sprawl Checks

```bash
# Run all default checks
pyrx check --project .

# Run all 16 checks and emit SARIF 2.1.0
pyrx check --project . --checks all --format sarif --output results.sarif

# Control CI exit codes globally or per-check via .pyrx.toml
pyrx check --project . --fail-on warning

# Pass per-check behavioral options on the CLI (or list all options via --check-options)
pyrx check --project . -O uncaught-exception-escape.check_main=true -O unused-parameter.check_constant_args=false
pyrx check --check-options

# Ratchet legacy findings with a baseline or diff-gate on a PR patch
pyrx check --project . --write-baseline .pyrx-baseline.json
pyrx check --project . --baseline .pyrx-baseline.json --patch-file pr.diff
```

#### All 16 Checks Across 5 Categories ([Full Documentation Index](docs/checks/README.md))
- **Category A — Anti-Sprawl & LLM Consolidation**:
  - [`semantic-clone`](docs/checks/semantic-clone.md): Functions/methods across modules with identical or near-identical alpha-renamed AST/CFG skeletons (`semantic-clone.min_statements = 5`).
  - [`schema-drift`](docs/checks/schema-drift.md): Parallel `@dataclass`, `TypedDict`, `NamedTuple`, or Pydantic `BaseModel` schemas with overlapping or conflicting field types.
  - [`dict-contract-mismatch`](docs/checks/dict-contract-mismatch.md): Cross-module producer dict literal keys vs unguarded consumer `Load` subscript accesses (`KeyError` hazards).
  - [`uncentralized-env-access`](docs/checks/uncentralized-env-access.md) *(legacy alias: `env-sprawl`)*: Scattered `os.environ` / `os.getenv` reads outside configuration modules (`config`, `settings`, `env`, `constants`, `cli`, `__main__`, `conftest`, or `config_modules` in `.pyrx.toml`) or conflicting defaults/casts.
- **Category B — Reachability & Dead Subgraphs**:
  - [`dead-code`](docs/checks/dead-code.md): Unreachable functions, classes, methods, and mutually-referencing dead helper subgraphs (recognizing `__all__`, `__init__.py` re-exports, Python dunder protocols, nested closures, and `[entry_points]` in `.pyrx.toml`).
  - [`unused-parameter`](docs/checks/unused-parameter.md) *(legacy alias: `unused-parameter-cross-tu`)*: Internal function parameters unread in body or passed identical constants at every call site (exempting dunders, `Protocol`/`ABC`, stubs, and signal handlers; configurable via `unused-parameter.check_dunders` and `unused-parameter.check_constant_args`).
  - [`test-only-production-code`](docs/checks/test-only-production-code.md): Non-test symbols whose only callers across the entire repository live under `tests/`.
- **Category C — Interprocedural Exception & Error Boundaries**:
  - [`uncaught-exception-escape`](docs/checks/uncaught-exception-escape.md): Transitive exceptions escaping a declared CLI/RPC/skill entry point without a matching `try`/`except` (`uncaught-exception-escape.check_main = false` by default).
  - [`broad-catch-swallow`](docs/checks/broad-catch-swallow.md): Broad `except Exception:` / bare `except:` blocks that swallow errors or `asyncio.CancelledError`.
  - [`exception-contract-divergence`](docs/checks/exception-contract-divergence.md): Protocol/ABC method overrides with conflicting raise vs `None`-return contracts (including `file:line` of the base contract).
- **Category D — Async, Concurrency & Resource Lifetimes**:
  - [`blocking-in-async`](docs/checks/blocking-in-async.md): `async def` coroutines transitively calling blocking I/O, `subprocess`, or `time.sleep`, with concrete non-blocking replacement examples.
  - [`lock-order-inversion`](docs/checks/lock-order-inversion.md): Interprocedural lock acquisition cycles (`L1 -> L2` vs `L2 -> L1`) or `await` while holding `threading.Lock`.
  - [`unowned-resource-return`](docs/checks/unowned-resource-return.md): Returning `open(...)`, `NamedTemporaryFile`, `Popen`, or `httpx.Client` without caller `with` / `ExitStack` ownership.
- **Category E — Import Graph & Module Hygiene**:
  - [`import-side-effects`](docs/checks/import-side-effects.md): Module-top-level (`<module>`) subprocess, network, or file-write side effects executed at `import` time.
  - [`import-cycle`](docs/checks/import-cycle.md): Strongly connected components (SCCs) in the runtime import graph outside `if TYPE_CHECKING:`.
  - [`heavy-import-leak`](docs/checks/heavy-import-leak.md): Lightweight modules importing heavyweight packages (`torch`, `tensorflow`, `pandas`, etc.) at top level without Python 3.15 `lazy import`.

#### Configuration (`.pyrx.toml` or `[tool.pyrx]` in `pyproject.toml`)

```toml
# Global exit-code threshold: "error" | "warning" | "note" | "none"
fail_on = "warning"
checks = ["all"]
config_modules = ["config", "settings", "env", "my_pkg.bootstrap"]
entry_points = ["my_pkg.plugins:*", "my_pkg.public_api"]

# Per-check severity, CI exit-code, and behavioral option overrides
[rules.heavy-import-leak]
severity = "note"
fail = false  # Report in output/SARIF without causing a nonzero CI exit code

[check_options]
"uncaught-exception-escape.check_main" = false
"unused-parameter.check_dunders" = false
"unused-parameter.check_constant_args" = true
"semantic-clone.min_statements" = 5
```

Inline suppressions use `# pyrx: ignore[check-name]` (or `# pyrex: ignore[check-name]`).

---

### 3. `pyrx morph` — Multi-File CST Refactoring & Consolidation

```bash
# Preview consolidating duplicate helpers into a canonical definition
pyrx morph consolidate --project . --canonical my_pkg.utils.normalize_slug --dry-run

# Apply declarative codemods conditioned on megascope facts (e.g. callerExecutionContext == "async")
pyrx morph rewrite --project . --rules-json rules.json --dry-run
```

---

## Exit Codes

| Code | Meaning |
| :---: | :--- |
| `0` | Clean check run, or non-empty `megascope` query result |
| `1` | Check findings reported at/above `--fail-on` (and not configured with `fail = false`), or empty query result / `not_found` |
| `2` | CLI usage or configuration error |
| `3` | Index error, corrupted `.pyrx` checksum, or parse failure |
| `4` | Ambiguous symbol identity (`status: "ambiguous"` with candidate USRs) |
