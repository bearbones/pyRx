# Buck2 Integration for `pyrex`

`buck2/pyrex.bzl` and `buck2/pyrex.bxl` integrate `pyrex`'s whole-program Python indexing (`megascope`) and cross-module static analysis (`check`) into Buck2's hermetic action graph using a 3-tier map-reduce pipeline:

1. **`pyrex_module_index`**: Indexes a single `.py` file into a deterministic `<module>.pyrx` shard (`pyrex megascope index --threads 1 --source <src.py> --index <mod.pyrx>`).
2. **`pyrex_target_index`**: Merges per-module shards for a `python_library` or `python_binary` into `<target>.pyrx` (`pyrex megascope merge`) and propagates a `PyrexIndexTSet` (`transitive_set`) across `deps`.
3. **`pyrex_global_index`**: Reduces `PyrexIndexTSet` across any target graph into a single global `.pyrx` index with diamond-dependency deduplication.
4. **`pyrex_query` & `pyrex_check`**: Run `pyrex megascope <tool>` queries or `pyrex check` SARIF/JSON/text gates directly against any target or global `.pyrx` index.
5. **Drop-In Wrapper Macros**: `pyrex_python_library` and `pyrex_python_binary` wrap standard Buck2 Python rules and automatically attach `<name>_pyrex` targets.
6. **Zero-Config BXL (`buck2/pyrex.bxl`)**: Run `buck2 bxl //buck2:pyrex.bxl:index -- --targets //pkg/...` or `buck2 bxl //buck2:pyrex.bxl:check -- --targets //pkg/...` over unmodified `BUCK` files.
