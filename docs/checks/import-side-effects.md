# `import-side-effects`

- **Category**: `imports` (Category E — Import Graph & Module Hygiene)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects module top-level (`<module>`) statements outside `if __name__ == "__main__":` guards that directly or transitively execute impure side effects at `import` time:
- Running subprocesses (`subprocess.run`, `subprocess.Popen`, `os.system`)
- Performing network requests (`requests.get`, `httpx.get`, `urllib.request.urlopen`)
- Writing or deleting files/directories (`Path.write_text`, `os.remove`, `shutil.rmtree`)
- Spawning background threads or processes (`threading.Thread.start`, `multiprocessing.Process.start`)

Import-time side effects slow down CLI startup, break `pytest` collection hermeticity, and can fail when a module is imported solely for type introspection or discovery.

## Example

```python
# pkg/plugin.py
import subprocess

# Executed immediately whenever pkg.plugin is imported!
GIT_SHA = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
```

### Fixed

Defer the side effect into an explicit function or `@functools.lru_cache` accessor called at runtime:

```python
from functools import lru_cache
import subprocess

@lru_cache(maxsize=1)
def get_git_sha() -> str:
    return subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
```

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.import-side-effects]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[import-side-effects]`
