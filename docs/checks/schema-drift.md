# `schema-drift`

- **Category**: `sprawl` (Category A — Anti-Sprawl & LLM Consolidation)
- **Default**: Enabled (`on`)
- **Default Severity**: `warning`

## What It Detects

Detects `@dataclass`, `TypedDict`, `NamedTuple`, or Pydantic `BaseModel` schemas across modules that represent the same domain concept (matching class name or $\ge 75\%$ field overlap with $\ge 3$ shared fields) but diverge in field names or field type annotations.

## Example

```python
# pkg/alpha.py
from dataclasses import dataclass

@dataclass
class UserProfile:
    user_id: int
    email: str
    active: bool

# pkg/beta.py
from dataclasses import dataclass

@dataclass
class UserProfile:
    user_id: str      # Type conflict: int vs str
    email: str
    is_admin: bool    # Missing 'active', added 'is_admin'
```

## How to Fix

Extract the canonical domain model into a shared schema module (e.g. `pkg/models.py` or `pkg/schemas.py`) and import it across modules. If two distinct concepts share a name by coincidence, rename the classes to reflect their specific boundary role (e.g. `UserProfileRow` vs `UserProfilePayload`).

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.schema-drift]
severity = "warning"
fail = true
```

Inline suppression: `# pyrx: ignore[schema-drift]`
