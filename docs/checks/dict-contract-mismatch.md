# `dict-contract-mismatch`

- **Category**: `sprawl` (Category A — Anti-Sprawl & LLM Consolidation)
- **Default**: Enabled (`on`)
- **Default Severity**: `error`

## What It Detects

Detects cross-module `KeyError` hazards where a producer function returns a `dict` literal with static string keys `[k1, k2, ...]` and a caller (or consumer) accesses a required key `result["missing_key"]` that is not present in the producer's returned dictionary shape.

## Example

```python
# pkg/producer.py
def fetch_payload() -> dict:
    return {"status": 200, "user_id": 42}

# pkg/consumer.py
from pkg.producer import fetch_payload

def process() -> str:
    data = fetch_payload()
    return data["username"]  # KeyError: producer returns ['status', 'user_id']
```

## How to Fix

1. Align the key names between the producer and consumer, or use `.get("username")` if the key is optional.
2. Prefer replacing ad-hoc cross-module `dict` payloads with a `@dataclass`, `TypedDict`, or Pydantic `BaseModel` so attribute access is statically checked.

## Precision & Guard Recognition

`dict-contract-mismatch` only inspects **unguarded `Load` subscripts** (`d["k"]` in read context). It automatically excludes:
- Dictionary writes and mutations (`d["k"] = val`, `d["k"] += 1`, `del d["k"]`),
- Subsequent reads of keys written earlier in the same function (`d["k"] = val; return d["k"]`), and
- Guarded accesses inside `if "k" in d:`, `if "k" not in d:` (with early return/raise/continue/break), `if d.get("k"):`, `assert "k" in d`, or ternary expressions `d["k"] if "k" in d else default`.

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.dict-contract-mismatch]
severity = "error"
fail = true
```

Inline suppression: `# pyrx: ignore[dict-contract-mismatch]`
