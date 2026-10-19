# `blocking-in-async`

- **Category**: `async` (Category D — Async, Concurrency & Resource Lifetimes)
- **Default**: Enabled (`on`)
- **Default Severity**: `error`

## What It Detects

Detects `async def` coroutines that directly or transitively call synchronous blocking operations across helper functions without offloading them via `await asyncio.to_thread(...)` or `loop.run_in_executor(...)`.

Calling blocking I/O, `time.sleep`, or synchronous `subprocess` functions inside an `async def` freezes the entire `asyncio` event loop thread, stalling all concurrent tasks, timeouts, and heartbeats—even when the blocking call is hidden several hops deep inside a synchronous helper function.

## Concrete Non-Blocking Replacements

Each `blocking-in-async` diagnostic includes a concrete non-blocking replacement tailored to the detected blocking API:

| Blocking Call | Recommended Non-Blocking Replacement |
| :--- | :--- |
| `time.sleep(secs)` | `await asyncio.sleep(secs)` |
| `open(path, ...)` / `Path.read_text()` / `Path.write_text()` | `async with aiofiles.open(path, ...) as f:` or `await asyncio.to_thread(Path(path).read_text)` |
| `requests.get(...)` / `requests.post(...)` / `urllib.request.urlopen(...)` | `async with httpx.AsyncClient() as client: resp = await client.get(...)` (or `aiohttp.ClientSession`) |
| `subprocess.run(...)` / `subprocess.check_output(...)` / `subprocess.Popen(...)` | `proc = await asyncio.create_subprocess_exec(...); await proc.communicate()` (or `await asyncio.to_thread(subprocess.run, ...)`) |
| `socket.*` / synchronous DB or CPU/I/O helper | `await asyncio.to_thread(blocking_fn, *args)` |

## Example

```python
# pkg/alpha.py
import time

def sync_sleep_helper() -> None:
    time.sleep(1)

# pkg/beta.py
from pkg.alpha import sync_sleep_helper

async def async_endpoint() -> int:
    # Transitive blocking call: async_endpoint -> sync_sleep_helper -> time.sleep
    sync_sleep_helper()
    return 1
```

### Fixed

```python
import asyncio

async def async_endpoint() -> int:
    await asyncio.sleep(1)
    return 1
```

Or when calling a legacy synchronous helper that cannot be made `async`:

```python
import asyncio
from pkg.alpha import sync_sleep_helper

async def async_endpoint() -> int:
    await asyncio.to_thread(sync_sleep_helper)
    return 1
```

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.blocking-in-async]
severity = "error"
fail = true
```

Inline suppression: `# pyrx: ignore[blocking-in-async]`
