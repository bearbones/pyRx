# `exception-contract-divergence`

- **Category**: `exceptions` (Category C — Interprocedural Exception & Error Boundaries)
- **Default**: Enabled (`on`)
- **Default Severity**: `error`

## What It Detects

Detects Liskov Substitution Principle (LSP) violations in exception and error-return contracts across `Protocol` or `ABC` method hierarchies:
1. **Unexpected Exception Types**: An override raises concrete exception types (excluding `NotImplementedError`) that are not raised by the base contract or sibling implementations.
2. **Raise vs. `None`-Return Divergence**: One implementation raises an exception on missing/invalid data while a sibling implementation of the same base contract returns `None` without raising.

Every diagnostic includes the relative `file:line` source location of the base contract method (for example, `'httpx._auth.Auth.auth_flow' (httpx/_auth.py:37)`) so you can immediately inspect the interface definition.

## Example

```python
# pkg/base.py
class StorageBackend:
    def fetch(self, key: str) -> str:  # pkg/base.py:2
        raise NotImplementedError

# pkg/memory.py
from pkg.base import StorageBackend

class MemoryStorage(StorageBackend):
    def fetch(self, key: str) -> str:
        return None  # Returns None on missing key

# pkg/disk.py
from pkg.base import StorageBackend

class DiskStorage(StorageBackend):
    def fetch(self, key: str) -> str:
        raise KeyError(key)  # Diverges from base contract 'pkg.base.StorageBackend.fetch' (pkg/base.py:2) and sibling MemoryStorage.fetch
```

### Diagnostic Output

```text
pkg/disk.py:5:5: error: [exception-contract-divergence] Override 'pkg.disk.DiskStorage.fetch' raises [KeyError] not raised by base contract 'pkg.base.StorageBackend.fetch' (pkg/base.py:2) or sibling implementations
```

## How to Fix

1. Standardize the error contract across all implementations of the base class or `Protocol` (either all implementations raise a shared domain exception declared on the base contract, or all return `Optional[T]` / `None`).
2. Document or declare the expected domain exception on the base contract method.

## Configuration & Suppression

```toml
# .pyrx.toml
[rules.exception-contract-divergence]
severity = "error"
fail = true
```

Inline suppression: `# pyrx: ignore[exception-contract-divergence]`
