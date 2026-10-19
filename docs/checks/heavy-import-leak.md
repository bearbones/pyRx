# `heavy-import-leak`

- **Category**: `imports` (Category E — Import Graph & Module Hygiene)
- **Default**: Disabled (`off`, enable via `--checks all` or `checks = ["heavy-import-leak"]`)
- **Default Severity**: `warning`

## What It Detects

Detects lightweight non-test modules that directly or transitively import heavyweight third-party packages (`torch`, `tensorflow`, `pandas`, `scipy`, `sklearn`, `matplotlib`, `cv2`, `grpc`, `boto3`, `sqlalchemy`, `playwright`, `selenium`, or custom `heavy_modules`) at module top-level load time (`is_lazy == false` and outside `if TYPE_CHECKING:`).

Importing heavyweight packages eagerly at module top level adds hundreds of milliseconds (or seconds) to CLI startup (`--help`, tab-completion, lightweight subcommands) and test collection even when the heavy functionality is never invoked on that execution path.

## Recommended Fixes

### 1. Python 3.15 `lazy import` Statement (PEP 810)

Python 3.15 introduces native [`lazy import` statements](https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports), which defer module loading until the imported binding is first referenced at runtime while keeping imports cleanly at the top of the file:

```python
# Python 3.15+ native lazy imports (https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports)
lazy import torch
lazy from pandas import DataFrame
```

`pyRx` inspects `requires-python` in your project's `pyproject.toml`:
- If your project already targets Python 3.15+ (`requires-python = ">=3.15"`), `pyRx` recommends converting the top-level import to `lazy import <module>`.
- If your project targets an older Python version (e.g. `requires-python = ">=3.12"`) or does not specify `>=3.15`, `pyRx` recommends upgrading the project to Python 3.15 to use native `lazy import` (`https://docs.python.org/3.15/reference/simple_stmts.html#lazy-imports`), or moving the import inside the specific function(s) that require it.

### 2. Function-Local Import or `if TYPE_CHECKING:`

On Python `< 3.15`, guard type-only imports with `if TYPE_CHECKING:` and move runtime imports inside the function that uses them:

```python
from __future__ import annotations
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import torch

def run_inference(weights_path: str) -> list[float]:
    import torch  # Loaded only when run_inference() is called
    model = torch.load(weights_path)
    return model.predict()
```

## CI Exit-Code Configurability (`.pyrx.toml` & `--fail-on`)

Because `heavy-import-leak` is often used as an advisory performance check in CI, you can configure whether it causes a nonzero exit code (`1`) either globally via `--fail-on` or per-check in `.pyrx.toml`:

```toml
# .pyrx.toml
# Customize the list of heavyweight packages
heavy_modules = ["torch", "tensorflow", "pandas", "scipy", "grpc", "boto3"]

[rules.heavy-import-leak]
enabled = true
severity = "warning"
fail = false  # Report findings in text/JSON/SARIF output without failing CI (exit code 0)
```

Or on the command line:

```bash
# Only fail CI on error-severity findings (warnings and notes exit 0)
pyrx check --project . --checks all --fail-on error
```

Inline suppression: `# pyrx: ignore[heavy-import-leak]`
