pub mod cache;
pub mod check;
mod frameworks;
pub mod indexer;
pub mod morph;
pub mod workspace;

pub use megascope::{diff, ffi, graph, model, query, snapshot};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use crate::graph::ProgramIndex;
use crate::model::ModuleShard;
use crate::workspace::{SourceSelectionOptions, select_sources};
use rayon::prelude::*;

/// Indexes selected workspace sources in parallel (using `rayon`) into a deterministic `ProgramIndex`.
pub fn bake_program_index(
    sel_opts: &SourceSelectionOptions,
    collapse_paths: Vec<String>,
    threads: usize,
) -> Result<ProgramIndex, String> {
    bake_program_index_with_stats(sel_opts, collapse_paths, threads).map(|(index, _)| index)
}

pub fn bake_program_index_with_stats(
    sel_opts: &SourceSelectionOptions,
    collapse_paths: Vec<String>,
    threads: usize,
) -> Result<(ProgramIndex, cache::CacheStats), String> {
    let (ws, modules) = select_sources(sel_opts)?;
    if modules.is_empty() {
        return Err("No Python source files matched selection".to_string());
    }

    let cache = cache::ModuleCache::new(&ws, !sel_opts.no_cache);
    let mut shards: Vec<ModuleShard> = if threads == 1 || modules.len() == 1 {
        modules.iter().map(|m| cache.index(m, &ws)).collect()
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| format!("Failed to initialize thread pool: {e}"))?;
        pool.install(|| modules.par_iter().map(|m| cache.index(m, &ws)).collect())
    };

    frameworks::mark_entry_points(&mut shards, &ws);
    propagate_package_exports(&mut shards);

    let project_root_str = ws.project_root.to_string_lossy().replace('\\', "/");
    Ok((
        ProgramIndex::from_shards(shards, project_root_str, ws.entry_points, collapse_paths),
        cache.stats(),
    ))
}

/// Propagates explicit exports (`__all__` re-exports and package `__init__.py` re-exports)
/// across module shards so exported library symbols and their public methods are marked as entry points.
pub fn propagate_package_exports(shards: &mut [ModuleShard]) {
    use crate::model::EntityKind;
    use std::collections::{BTreeMap, BTreeSet};

    // Map module_name -> shard index
    let mut mod_to_idx: BTreeMap<String, usize> = BTreeMap::new();
    for (idx, s) in shards.iter().enumerate() {
        mod_to_idx.insert(s.module_name.clone(), idx);
    }

    // Collect (target_module, target_symbol) pairs exported via __all__ or __init__.py re-exports
    let mut exported_targets: BTreeSet<(String, String)> = BTreeSet::new();
    for shard in shards.iter() {
        let is_init = shard.file == "__init__.py" || shard.file.ends_with("/__init__.py");
        if let Some(dunder) = &shard.dunder_all {
            for exp_name in dunder {
                if let Some((target_mod, Some(orig_sym))) = shard.import_bindings.get(exp_name) {
                    if !target_mod.is_empty() {
                        exported_targets.insert((target_mod.clone(), orig_sym.clone()));
                    }
                }
            }
        } else if is_init {
            for imp in &shard.imports {
                if imp.is_type_checking || imp.is_lazy {
                    continue;
                }
                if let Some(sym) = &imp.imported_symbol {
                    let exposed = imp.alias.as_deref().unwrap_or(sym);
                    if (sym == "*" || !exposed.starts_with('_')) && !imp.target_module.is_empty() {
                        exported_targets.insert((imp.target_module.clone(), sym.clone()));
                    }
                }
            }
        }
    }

    // Follow multi-hop re-export chains (e.g. pkg/__init__.py -> pkg/sub/__init__.py -> pkg/sub/mod.py)
    loop {
        let current: Vec<(String, String)> = exported_targets.iter().cloned().collect();
        let mut added = false;
        for (tmod, tsym) in current {
            if let Some(&s_idx) = mod_to_idx.get(&tmod) {
                let shard = &shards[s_idx];
                if tsym == "*" {
                    for (local_name, (next_mod, next_sym_opt)) in &shard.import_bindings {
                        if let Some(next_sym) = next_sym_opt {
                            if !local_name.starts_with('_') && !next_mod.is_empty() {
                                if exported_targets.insert((next_mod.clone(), next_sym.clone())) {
                                    added = true;
                                }
                            }
                        }
                    }
                } else if let Some((next_mod, Some(next_sym))) = shard.import_bindings.get(&tsym) {
                    if !next_mod.is_empty() {
                        if exported_targets.insert((next_mod.clone(), next_sym.clone())) {
                            added = true;
                        }
                    }
                }
            }
        }
        if !added {
            break;
        }
    }

    for shard in shards.iter_mut() {
        let mut exported_classes = BTreeSet::new();
        for ent in &mut shard.entities {
            if ent.kind == EntityKind::Module {
                continue;
            }
            if ent.enclosing_class.is_none() {
                let reexported = exported_targets
                    .contains(&(shard.module_name.clone(), ent.name.clone()))
                    || (!ent.name.starts_with('_')
                        && exported_targets
                            .contains(&(shard.module_name.clone(), "*".to_string())));
                if reexported {
                    ent.is_exported = true;
                    if !ent.is_entry_point {
                        ent.is_entry_point = true;
                        ent.entry_point_kind = Some("package_export".to_string());
                    }
                }
                if ent.kind == EntityKind::Class && (ent.is_exported || ent.is_entry_point) {
                    exported_classes.insert(ent.qualified_name.clone());
                }
            }
        }

        if !exported_classes.is_empty() {
            for ent in &mut shard.entities {
                if let Some(cls) = &ent.enclosing_class {
                    if exported_classes.contains(cls) && !ent.name.starts_with('_') {
                        ent.is_exported = true;
                        if !ent.is_entry_point {
                            ent.is_entry_point = true;
                            ent.entry_point_kind = Some("package_export".to_string());
                        }
                    }
                }
            }
        }
    }
}
