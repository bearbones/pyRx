# `dead-code`

- **Category**: `reachability` (Category B — Whole-Program Reachability & Dead Subgraphs)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects functions, methods, classes, and mutually-referencing helper subgraphs that are unreachable from any entry point or public package export across the entire repository.

Unlike single-file linters that only see unused local imports or private functions, `pyRx` builds the whole-program call and MRO/Protocol override graph. Two dead functions `dead_cycle_a()` and `dead_cycle_b()` that call each other—or an internal helper imported into another file whose caller was deleted—are accurately identified as unreachable.

## How Entry Points and Public Exports Are Recognized

`pyRx` treats the following symbols (and everything transitively reachable from them) as live roots:
1. **Explicit Module Exports (`__all__`)**: Any symbol listed in `__all__ = [...]` (including `__all__ += [...]`, `__all__.extend(...)`, or `__all__.append(...)`) inside its defining module or re-exporting package.
2. **Package `__init__.py` Public Exports**: Public symbols defined in or re-exported from a package `__init__.py` (`from .submodule import PublicClass, public_helper`), plus public methods of exported classes.
3. **Project & Framework Entry Points**:
   - `[project.scripts]` and `[project.gui-scripts]` in `pyproject.toml`
   - `if __name__ == "__main__":` blocks and `main()` functions
   - `pytest` test functions (`test_*`), fixtures (`@pytest.fixture`), and hooks (`conftest.py`)
   - Web/CLI framework route and command decorators (`@app.get`, `@router.post`, `@click.command`, `@typer.command`, `@celery.task`, etc.)
   - `SKILL.md` referenced scripts
   - Protocol/ABC method overrides of reachable base methods
   - All Python data-model dunder/protocol methods (`__len__`, `__getitem__`, `__setitem__`, `__contains__`, `__missing__`, `__deepcopy__`, `__getstate__`, `__setstate__`, `__class_getitem__`, `__init_subclass__`, etc.) on reachable classes, as well as PEP 562 module-level `__getattr__` and `__dir__` hooks
   - Nested local functions/closures (`def wrapper(...)`) called within or returned from their enclosing function
4. **Configured `[entry_points]` in `.pyrx.toml`**: Any module, class, function, or wildcard pattern (`"my_pkg.plugins:*"`) configured in `.pyrx.toml` or via `--entry-point`.

> **Note on Test-Only Callers**: If a production symbol is reachable *only* from test modules (`tests/`), it is not flagged as `dead-code`; instead, it is reported by [`test-only-production-code`](test-only-production-code.md) unless exported as part of the public API (`__all__`, `__init__.py`, or `[entry_points]`).

## Example

```python
# pkg/internal.py
def used_helper() -> int:
    return 42

def dead_cycle_a(n: int) -> int:
    return dead_cycle_b(n - 1) if n > 0 else 0

def dead_cycle_b(n: int) -> int:
    return dead_cycle_a(n - 1)
```

### Exporting Intended Public API Symbols

If a function or class is part of your library's external interface (and therefore has no internal callers in the repository), export it explicitly:

```python
# Option 1: Declare in module __all__
__all__ = ["PublicClient", "public_helper"]

# Option 2: Re-export in package __init__.py
# pkg/__init__.py
from .internal import PublicClient, public_helper
```

Or declare it in `.pyrx.toml`:

```toml
# .pyrx.toml
entry_points = [
    "pkg.internal.PublicClient",
    "pkg.plugins:*",
]
```

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.dead-code]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[dead-code]`
