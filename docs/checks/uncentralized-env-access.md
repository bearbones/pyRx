# `uncentralized-env-access`

- **Category**: `sprawl` (Category A — Anti-Sprawl & LLM Consolidation)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`
- **Legacy Alias**: `env-sprawl`

## What It Detects

Detects scattered `os.environ` / `os.getenv` reads across non-configuration modules, or inconsistent default values and type casts (`int(...)`, `bool(...)`, `str`) for the same environment variable across multiple modules.

When environment variables are read ad-hoc throughout business logic modules:
- Default values and type conversions drift silently (`os.getenv("API_TIMEOUT", "30")` in one module vs `int(os.getenv("API_TIMEOUT", "10"))` in another).
- Tests become harder to isolate because hidden `os.environ` dependencies are not declared in a central configuration boundary.

## How Configuration Modules Are Determined

`pyRx` classifies a module as a **configuration module** (where reading `os.environ` / `os.getenv` is expected) if any of the following hold:
1. Any dot-separated segment of its module path or its file stem matches one of the built-in configuration module names:
   - `config` (or `*_config`, `config_*`)
   - `settings` (or `*_settings`, `settings_*`)
   - `env` (or `*_env`, `env_*`)
   - `constants`
   - `cli`
   - `__main__`
   - `conftest`
2. Its module name or file path matches any custom pattern listed in `config_modules` in `.pyrx.toml` (or `[tool.pyrx]` in `pyproject.toml`).

Any module that does not match these criteria is treated as a **non-config module**.

## Example

```python
# pkg/alpha.py (non-config module)
import os

def call_service():
    timeout = os.getenv("API_TIMEOUT", "30")  # Flagged: read outside config module

# pkg/beta.py (non-config module)
import os

def poll_service():
    timeout = int(os.getenv("API_TIMEOUT", "10"))  # Flagged: conflicting default ("10" vs "30") and cast (int vs str)
```

### Fixed

```python
# pkg/config.py (configuration module)
import os

API_TIMEOUT_SECONDS: int = int(os.getenv("API_TIMEOUT", "30"))

# pkg/alpha.py
from pkg.config import API_TIMEOUT_SECONDS
```

## Configuration & Suppression

You can customize which modules are recognized as configuration boundaries or adjust severity/CI failure behavior in `.pyrx.toml`:

```toml
# .pyrx.toml
config_modules = ["config", "settings", "env", "constants", "cli", "__main__", "conftest", "pkg.bootstrap"]

[rules.uncentralized-env-access]
severity = "warning"
fail = false  # Report without causing a nonzero exit code in CI
```

Inline suppression: `# pyrx: ignore[uncentralized-env-access]` (or `# pyrx: ignore[env-sprawl]`)
