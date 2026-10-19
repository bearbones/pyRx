# `unowned-resource-return`

- **Category**: `async` (Category D — Async, Concurrency & Resource Lifetimes)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects functions that allocate a context-managed resource (`open(...)`, `tempfile.NamedTemporaryFile(delete=False)`, `subprocess.Popen(...)`, `httpx.Client(...)`, `sqlite3.connect(...)`) and return the raw handle across module boundaries without `@contextlib.contextmanager` and without callers wrapping the call in a `with` statement or `contextlib.ExitStack`.

## Example

```python
# pkg/alpha.py
def allocate_raw_handle(path: str):
    return open(path, "w")  # Returned without @contextmanager

# pkg/beta.py
from pkg.alpha import allocate_raw_handle

def leak_handle() -> None:
    f = allocate_raw_handle("/tmp/out.txt")  # Not wrapped in 'with' or 'ExitStack'
    f.write("hello")
```

## How to Fix

Wrap the resource-allocating helper with `@contextlib.contextmanager` (and `yield` the handle inside a `with` block), or ensure every caller enters the returned context manager via `with`:

```python
from contextlib import contextmanager

@contextmanager
def open_output_handle(path: str):
    with open(path, "w", encoding="utf-8") as handle:
        yield handle
```

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.unowned-resource-return]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[unowned-resource-return]`
