# `semantic-clone`

- **Category**: `sprawl` (Category A — Anti-Sprawl & LLM Consolidation)
- **Default**: Enabled (`on`)
- **Default Severity**: `note` (informational; does not fail the default check run)

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
    slug = cleaned.replace(" ", "-")
    return slug

# pkg/beta.py
def normalize_slug(value: str) -> str:
    s = value.strip().lower()
    if not s:
        raise ValueError("empty")
    result = s.replace(" ", "-")
    return result
```

## How to Interpret

This is a review suggestion, not a correctness warning or a proof of equivalence. Normalization erases literal values and parameter names, so two functions can match while implementing different behavior. Review literals, defaults, decorators, captured state, keyword arguments, and external callers before sharing an implementation.

The diagnostic does not generate a fix. Automatic consolidation is deliberately unsupported. Keep separate functions when that better expresses the domain or allows them to evolve independently.

## Per-Check Options

| Option | Default | Description |
| :--- | :---: | :--- |
| `semantic-clone.min_statements` | `5` | Minimum statement count in a function body before normalized AST structural clone matching applies. |

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.semantic-clone]
severity = "note"
min_statements = 5
```

CLI override: `pyrx check -O semantic-clone.min_statements=6`

Inline suppression: `# pyrx: ignore[semantic-clone]`
