# `broad-catch-swallow`

- **Category**: `exceptions` (Category C — Interprocedural Exception & Error Boundaries)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects broad `except Exception:`, `except BaseException:`, or bare `except:` handlers that:
1. Silently swallow exceptions (`pass`, `return None`, `return False`) without logging or re-raising, or
2. Catch `Exception` / `BaseException` inside an `async def` coroutine without re-raising `asyncio.CancelledError`, breaking structured concurrency and task cancellation.

## Example

```python
async def swallow_errors(x: str) -> int:
    try:
        return int(x)
    except Exception:
        return 0  # Swallows exception without logging or re-raising CancelledError
```

## How to Fix

1. Catch the narrowest concrete exception type possible (e.g. `except ValueError:`).
2. If catching `Exception` at a top-level boundary is intentional, log the exception (`logger.exception(...)` or `logging.error(...)`).
3. In `async def` functions, explicitly re-raise cancellation before handling general exceptions:

```python
import asyncio
import logging

logger = logging.getLogger(__name__)

async def parse_value(x: str) -> int:
    try:
        return int(x)
    except asyncio.CancelledError:
        raise
    except ValueError:
        logger.warning("Invalid integer: %r", x)
        return 0
```

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.broad-catch-swallow]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[broad-catch-swallow]`
