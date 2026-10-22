# `pyRx` Check Reference

`pyrx check` analyzes invariants across modules and across the whole-program call, import, MRO/Protocol, and control-flow graph. Every check produces deterministic `pyrxFingerprint/v1` finding hashes and can be emitted as text, JSON, or SARIF 2.1.0.

## All 16 Checks

### Category A — Anti-Sprawl & LLM Consolidation (`sprawl`)

| Check | Default | Severity | Summary |
| :--- | :---: | :---: | :--- |
| [`semantic-clone`](semantic-clone.md) | `on` | `note` | Duplicate or near-duplicate functions/methods across modules with identical alpha-renamed AST/CFG skeletons. |
| [`schema-drift`](schema-drift.md) | `on` | `warning` | Parallel `@dataclass`, `TypedDict`, `NamedTuple`, or Pydantic `BaseModel` schemas across modules with overlapping or conflicting field types. |
| [`dict-contract-mismatch`](dict-contract-mismatch.md) | `on` | `error` | Producer dict literal keys disagree with consumer subscript accesses (`d["k"]`) across module boundaries. |
| [`uncentralized-env-access`](uncentralized-env-access.md) | `on` | `warning` | Scattered `os.environ` / `os.getenv` reads outside configuration modules, or conflicting defaults/casts across files *(legacy alias: `env-sprawl`)*. |

### Category B — Whole-Program Reachability & Dead Subgraphs (`reachability`)

| Check | Default | Severity | Summary |
| :--- | :---: | :---: | :--- |
| [`dead-code`](dead-code.md) | `on` | `warning` | Functions, methods, classes, and mutually-recursive helper subgraphs unreachable from any entry point or public export (`__all__`, `__init__.py`, `[entry_points]`). |
| [`unused-parameter`](unused-parameter.md) | `off` | `note` | Internal function parameters unread in the function body or passed identical constants at every call site across the repository *(legacy alias: `unused-parameter-cross-tu`)*. |
| [`test-only-production-code`](test-only-production-code.md) | `on` | `warning` | Production symbols whose only callers across the entire repository live in test modules (`tests/`). |

### Category C — Interprocedural Exception & Error Boundaries (`exceptions`)

| Check | Default | Severity | Summary |
| :--- | :---: | :---: | :--- |
| [`uncaught-exception-escape`](uncaught-exception-escape.md) | `on` | `warning` | Transitive `raise` paths that escape a declared CLI, skill, or RPC entry point without a matching `try`/`except` handler. |
| [`broad-catch-swallow`](broad-catch-swallow.md) | `on` | `warning` | Broad `except Exception:` or bare `except:` blocks that swallow errors without logging/re-raising or fail to re-raise `asyncio.CancelledError`. |
| [`exception-contract-divergence`](exception-contract-divergence.md) | `on` | `error` | Protocol/ABC method overrides that raise exception types not raised by the base contract (`file:line`) or sibling implementations, or return `None` where siblings raise. |

### Category D — Async, Concurrency & Resource Lifetimes (`async`)

| Check | Default | Severity | Summary |
| :--- | :---: | :---: | :--- |
| [`blocking-in-async`](blocking-in-async.md) | `on` | `error` | `async def` coroutines that directly or transitively invoke blocking synchronous operations (`time.sleep`, `open`, `requests`, `subprocess.run`). |
| [`lock-order-inversion`](lock-order-inversion.md) | `on` | `error` | Interprocedural lock acquisition cycles (`L1 -> L2` vs `L2 -> L1`) or `await` statements while holding a non-async `threading.Lock`. |
| [`unowned-resource-return`](unowned-resource-return.md) | `on` | `warning` | Returning context-managed resources (`open()`, `NamedTemporaryFile(delete=False)`, `Popen`, `httpx.Client`) without caller `with` / `ExitStack` ownership. |

### Category E — Import Graph & Module Hygiene (`imports`)

| Check | Default | Severity | Summary |
| :--- | :---: | :---: | :--- |
| [`import-side-effects`](import-side-effects.md) | `on` | `warning` | Module top-level (`<module>`) code that executes subprocesses, network I/O, or file writes at `import` time. |
| [`import-cycle`](import-cycle.md) | `on` | `error` | Strongly connected components (SCCs) in the runtime import graph outside `if TYPE_CHECKING:`. |
| [`heavy-import-leak`](heavy-import-leak.md) | `off` | `warning` | Lightweight modules transitively importing heavyweight packages (`torch`, `tensorflow`, `pandas`, etc.) at module load time instead of using Python 3.15 `lazy import` or function-local imports. |

---

## Configuration & CI Exit-Code Control

`pyRx` reads configuration from `.pyrx.toml` (or `[tool.pyrx]` in `pyproject.toml`, `.pyrex.toml`, or `.pyrx-check.json`):

```toml
# Global CI exit-code threshold: "error" | "warning" | "note" | "none"
fail_on = "warning"

# Enable/disable checks or groups
checks = ["all", "-unused-parameter"]

# Customize configuration module patterns for uncentralized-env-access
config_modules = ["config", "settings", "env", "constants", "cli", "__main__", "conftest"]

# Declare additional public API entry points for dead-code / test-only-production-code
entry_points = ["my_pkg.plugins:*", "my_pkg.public_api"]

# Customize heavyweight module list for heavy-import-leak
heavy_modules = ["torch", "tensorflow", "pandas", "scipy", "grpc", "boto3"]

# Per-check overrides: customize severity and whether findings cause a nonzero CI exit code
[rules.heavy-import-leak]
enabled = true
severity = "warning"
fail = false  # Emit findings in reports/SARIF without failing CI

[rules.uncentralized-env-access]
severity = "note"
fail = false

# Per-check behavioral options (can also be placed directly under [rules.<check>])
[check_options]
"uncaught-exception-escape.check_main" = false
"unused-parameter.check_dunders" = false
"unused-parameter.check_constant_args" = true
"semantic-clone.min_statements" = 5
```

### Per-Check Options (`-O` / `--check-option`)

You can configure per-check behavioral options in `.pyrx.toml` (under `[check_options]` or `[rules.<check>]`), in `.pyrx-check.json` (`"check_options"`), or on the CLI using `-O` / `--check-option <check>.<key>=<value>` (or `--check-options` with no arguments to list all options):

| Option | Default | Description |
| :--- | :---: | :--- |
| `uncaught-exception-escape.check_main` | `false` | Inspect module top-level `<module>` and `if __name__ == "__main__":` script blocks in addition to declared entry points. |
| `unused-parameter.check_dunders` | `false` | Inspect Python dunder/protocol methods (`__exit__`, `__getitem__`, etc.) whose signatures are fixed by the Python data model. |
| `unused-parameter.check_constant_args` | `true` | Report parameters that receive the exact same constant literal across all `>= 2` call sites in the repository. |
| `semantic-clone.min_statements` | `5` | Minimum statement count in a function body before normalized AST structural clone matching applies. |
| `heavy-import-leak.heavy_modules` | *(builtin)* | Comma-separated list of heavyweight top-level package names to flag on eager import. |
| `uncentralized-env-access.allowed_modules` | *(builtin)* | Comma-separated list of configuration module stems allowed to read `os.environ` / `os.getenv`. |


### Inline Suppressions

Suppress a specific check on a line (or the immediately preceding comment line):

```python
os.getenv("LEGACY_FLAG")  # pyrx: ignore[uncentralized-env-access]
```

Both `# pyrx: ignore[...]` and `# pyrex: ignore[...]` are supported, as well as legacy check aliases (`env-sprawl` and `unused-parameter-cross-tu`).

## Evidence, limits, and incremental analysis

See [analysis behavior](../analysis.md) for compact versus complete paths, current-graph patch impact, cache invalidation, framework entry points, and static-analysis limitations. Notes are informational under the default `fail_on = "warning"` threshold.
