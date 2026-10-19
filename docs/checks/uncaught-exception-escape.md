# `uncaught-exception-escape`

- **Category**: `exceptions` (Category C — Interprocedural Exception & Error Boundaries)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Traces interprocedural `raise` statements across the whole-program call graph and detects concrete exception types (excluding `SystemExit`, `KeyboardInterrupt`, and `AssertionError`) that can propagate all the way out of a non-test CLI, skill, or service entry point without an enclosing `try`/`except` handler along the call chain.

## Example

```python
# pkg/alpha.py
def risky_parse(text: str) -> dict:
    if not text:
        raise ValueError("empty input")
    return {"status": "ok"}

# pkg/main.py
from pkg.alpha import risky_parse

def main() -> int:
    # Uncaught ValueError from risky_parse escapes entry point 'pkg.main.main'
    risky_parse("x")
    return 0
```

## How to Fix

Handle expected domain or validation errors at your CLI/RPC entry boundary and convert them into structured error responses or exit codes, or catch them around the specific call site:

```python
def main() -> int:
    try:
        risky_parse("x")
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0
```

## Per-Check Options

| Option | Default | Description |
| :--- | :---: | :--- |
| `uncaught-exception-escape.check_main` | `false` | Also inspect module top-level `<module>` and `if __name__ == "__main__":` script blocks (by default, only declared CLI/service/skill entry points are checked). |

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.uncaught-exception-escape]
severity = "warning"
fail = true
check_main = false
```

CLI override: `pyrx check -O uncaught-exception-escape.check_main=true`

Inline suppression: `# pyrx: ignore[uncaught-exception-escape]`
