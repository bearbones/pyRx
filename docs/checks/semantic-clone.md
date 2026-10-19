# `semantic-clone`

- **Category**: `sprawl` (Category A — Anti-Sprawl & LLM Consolidation)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects functions and methods across different modules that have identical or near-identical normalized AST/CFG skeletons (`skeleton_hash`) after alpha-renaming local variables and parameters.

AI coding assistants frequently re-implement utility helpers (`normalize_slug`, `retry_with_backoff`, `atomic_write_json`, `run_command`) inside individual feature modules instead of importing an existing helper. Because local variable names or type annotations may differ slightly, text-based search misses them; `semantic-clone` canonicalizes the control-flow and call structure to surface consolidation candidates.

## Example

```python
# pkg/alpha.py
def clean_slug(raw_text: str) -> str:
    cleaned = raw_text.strip().lower()
    if not cleaned:
        raise ValueError("empty")
    return cleaned.replace(" ", "-")

# pkg/beta.py
def normalize_slug(value: str) -> str:
    s = value.strip().lower()
    if not s:
        raise ValueError("empty")
    return s.replace(" ", "-")
```

## How to Fix

Consolidate the duplicate implementations into a single shared helper module and import it at each call site—or run `pyrx morph consolidate`:

```bash
pyrx morph consolidate --project . --canonical pkg.alpha.clean_slug --dry-run
```

## Per-Check Options

| Option | Default | Description |
| :--- | :---: | :--- |
| `semantic-clone.min_statements` | `5` | Minimum statement count in a function body before normalized AST structural clone matching applies. |

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.semantic-clone]
severity = "warning"
fail = true
min_statements = 5
```

CLI override: `pyrx check -O semantic-clone.min_statements=6`

Inline suppression: `# pyrx: ignore[semantic-clone]`
