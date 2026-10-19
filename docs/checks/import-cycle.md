# `import-cycle`

- **Category**: `imports` (Category E — Import Graph & Module Hygiene)
- **Default**: Enabled (`on`)
- **Default Severity**: `error`

## What It Detects

Detects strongly connected components (SCCs) in the top-level runtime module import graph (excluding imports guarded by `if TYPE_CHECKING:` or function-local lazy imports).

Runtime import cycles cause order-dependent `ImportError: cannot import name '...' from partially initialized module` crashes when a module is imported in a different entry-point order.

## Example

```python
# pkg/alpha.py
from pkg.beta import BetaType

class AlphaType:
    pass

# pkg/beta.py
from pkg.alpha import AlphaType

class BetaType:
    pass
```

## How to Fix

1. If an import is only needed for type annotations, guard it under `if TYPE_CHECKING:` (with `from __future__ import annotations`):

```python
from __future__ import annotations
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.beta import BetaType
```

2. Or extract shared types/interfaces into a lower-level module (e.g. `pkg/types.py`) that both `pkg.alpha` and `pkg.beta` import without cycling.

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.import-cycle]
severity = "error"
fail = true
```

Inline suppression: `# pyrx: ignore[import-cycle]`
