# `lock-order-inversion`

- **Category**: `async` (Category D — Async, Concurrency & Resource Lifetimes)
- **Default**: Enabled (`on`)
- **Default Severity**: `error`

## What It Detects

Detects two classes of concurrency deadlock hazards across `with` and `async with` lock scopes:
1. **Lock Order Inversion (AB-BA Deadlock)**: One code path acquires `lock_a` then (directly or transitively through a callee) acquires `lock_b`, while another code path acquires `lock_b` then `lock_a`.
2. **`await` Under Synchronous Lock**: An `async def` coroutine performs an `await` while holding a synchronous `threading.Lock` or `threading.RLock`, risking event-loop deadlock if another coroutine on the same event loop thread attempts to acquire the same lock.

## Example

```python
import threading

lock_a = threading.Lock()
lock_b = threading.Lock()

def path_one() -> None:
    with lock_a:
        with lock_b:
            pass

def path_two() -> None:
    with lock_b:
        with lock_a:  # Lock order inversion: lock_b -> lock_a vs lock_a -> lock_b
            pass
```

## How to Fix

1. Enforce a strict global acquisition order across all locks (e.g. always acquire `lock_a` before `lock_b`).
2. In `async def` functions, replace `threading.Lock()` with `asyncio.Lock()` (`async with async_lock:`), or release the `threading.Lock` before any `await` point.

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.lock-order-inversion]
severity = "error"
fail = true
```

Inline suppression: `# pyrx: ignore[lock-order-inversion]`
