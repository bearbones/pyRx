# `unused-parameter`

- **Category**: `reachability` (Category B — Whole-Program Reachability & Dead Subgraphs)
- **Default**: Disabled (`off`, enable via `--checks all` or `checks = ["unused-parameter"]`)
- **Default Severity**: `note`
- **Legacy Alias**: `unused-parameter-cross-tu`

## What It Detects

Detects parameters on internal functions and methods (functions with $\ge 2$ call sites across the repository that are not entry points or polymorphic overrides) where either:
1. The parameter is never read in the function body (`unread_Parameters`), or
2. Every call site across the entire codebase passes the exact same constant literal (`True`, `False`, `None`, numeric, or string literal) for that parameter.

## Backwards Compatibility & Interface Contracts (`_param` Convention)

Sometimes a parameter is intentionally unused in the function body because it must be preserved for:
- Backwards compatibility with existing callers (positional or keyword),
- Callback or hook signatures (`def on_event(event, _context):`), or
- Structural interface / duck-typing contracts.

In Python, prefixing the parameter name with a leading underscore (`_param`) signals to both human readers and `pyRx` that the parameter is intentionally unused—analogous to commenting out the parameter name (`Type /*param*/`) or marking it `[[maybe_unused]]` in C++. Parameters prefixed with `_` (or `unused_*`) are never flagged by `unused-parameter`.

## Example

```python
# pkg/alpha.py
def compute_bonus(amount: int, legacy_flag: bool) -> int:
    # 'legacy_flag' is never read in the body, and all callers pass False
    return amount * 2

# pkg/caller_a.py
compute_bonus(10, False)

# pkg/caller_b.py
compute_bonus(25, False)
```

### Fixed

Either remove the dead parameter from the signature and all call sites:

```python
def compute_bonus(amount: int) -> int:
    return amount * 2
```

Or, if the parameter must remain in the signature for caller compatibility, prefix it with `_`:

```python
def compute_bonus(amount: int, _legacy_flag: bool = False) -> int:
    return amount * 2
```

## Built-In Exemptions

By default, `unused-parameter` automatically exempts signatures that are fixed by external protocols or interfaces:
- Python data-model dunder methods (`__exit__(self, exc_type, exc_val, exc_tb)`, `__getitem__`, etc.), unless `unused-parameter.check_dunders = true` is set,
- Methods on `typing.Protocol` or `abc.ABC` classes, or methods decorated with `@abstractmethod` or `@overload`,
- Abstract/stub function bodies consisting solely of `...`, `pass`, `raise NotImplementedError(...)`, or a docstring followed by those, and
- Standard 2-parameter POSIX signal handlers `(signum, frame)` / `(sig, frame)`.

## Per-Check Options

| Option | Default | Description |
| :--- | :---: | :--- |
| `unused-parameter.check_dunders` | `false` | Inspect Python dunder/protocol methods (`__exit__`, `__getitem__`, etc.). |
| `unused-parameter.check_constant_args` | `true` | Report parameters that receive the exact same constant literal across all `>= 2` call sites. |

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.unused-parameter]
enabled = true
severity = "note"
fail = false
check_dunders = false
check_constant_args = true
```

CLI override: `pyrx check --checks unused-parameter -O unused-parameter.check_constant_args=false`

Inline suppression: `# pyrx: ignore[unused-parameter]` (or `# pyrx: ignore[unused-parameter-cross-tu]`)
