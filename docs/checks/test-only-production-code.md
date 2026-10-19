# `test-only-production-code`

- **Category**: `reachability` (Category B — Whole-Program Reachability & Dead Subgraphs)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects functions, methods, and classes defined in production modules (`src/` or non-test packages) that are reachable **only** from test entry points (`tests/`, `test_*.py`, `*_test.py`, `conftest.py`) and are not reachable from any production entry point or explicit public export (`__all__`, package `__init__.py` re-exports, or `[entry_points]` in `.pyrx.toml`).

When a production feature is refactored or removed, its unit tests often remain in `tests/`, keeping the orphaned production function "alive" to simple reference counters even though no production code path can ever invoke it. Conversely, test fixtures and assertion helpers sometimes leak into production modules instead of living under `tests/`.

## Example

```python
# pkg/alpha.py (production module)
def helper_only_for_tests() -> str:
    return "test-only"

# tests/test_alpha.py
from pkg.alpha import helper_only_for_tests

def test_helper():
    assert helper_only_for_tests() == "test-only"
```

## How to Fix

1. **If the symbol is a test helper**: Move it into `tests/` (e.g. `tests/conftest.py` or `tests/helpers.py`).
2. **If the symbol is obsolete production code**: Delete both the production symbol and its obsolete unit test.
3. **If the symbol is an intentional public library API**: Export it via `__all__ = ["..."]`, re-export it in your package's `__init__.py`, or list it under `entry_points` in `.pyrx.toml`.

## Configuration & Suppression

```toml
# .pyrx.toml
entry_points = ["pkg.public_api:*"]

[rules.test-only-production-code]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[test-only-production-code]`
