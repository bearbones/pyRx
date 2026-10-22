//! Reuse raw module facts, then always redo cross-module linking and export propagation.
use crate::indexer::{index_module, index_module_str};
use crate::model::{ModuleShard, ParseOutcome};
use crate::workspace::{SourceModule, WorkspaceConfig};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub reused: usize,
    pub parsed: usize,
}

#[derive(Serialize, Deserialize)]
struct CachedModule {
    key: u64,
    checksum: u64,
    shard: ModuleShard,
}

pub(crate) struct ModuleCache {
    directory: Option<PathBuf>,
    context: u64,
    reused: AtomicUsize,
    parsed: AtomicUsize,
}

impl ModuleCache {
    pub fn new(ws: &WorkspaceConfig, enabled: bool) -> Self {
        // Source-level identity also invalidates developer builds without a version bump.
        static FRONTEND: OnceLock<u64> = OnceLock::new();
        let frontend = FRONTEND.get_or_init(|| {
            xxh3_64(
                concat!(
                    env!("CARGO_PKG_VERSION"),
                    include_str!("indexer.rs"),
                    include_str!("workspace.rs"),
                    include_str!("cache.rs"),
                    include_str!("../Cargo.lock")
                )
                .as_bytes(),
            )
        });
        let context = xxh3_64(
            &serde_json::to_vec(&(frontend, &ws.entry_points, &ws.skill_entry_points)).unwrap(),
        );
        Self {
            directory: enabled.then(|| ws.project_root.join(".pyrx/cache")),
            context,
            reused: AtomicUsize::new(0),
            parsed: AtomicUsize::new(0),
        }
    }

    pub fn index(&self, module: &SourceModule, ws: &WorkspaceConfig) -> ModuleShard {
        let bytes = match fs::read(&module.abs_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.parsed.fetch_add(1, Ordering::Relaxed);
                return index_module(module, ws);
            }
        };
        let fingerprint = xxh3_64(&bytes);
        let key = xxh3_64(
            &serde_json::to_vec(&(
                self.context,
                fingerprint,
                &module.rel_path,
                &module.module_name,
                module.is_package_init,
                module.is_test_module,
            ))
            .unwrap(),
        );
        // One replaceable entry per source path bounds cache growth across edits.
        let path = self
            .directory
            .as_ref()
            .map(|dir| dir.join(format!("{:016x}.json", xxh3_64(module.rel_path.as_bytes()))));
        if let Some(path) = &path {
            if let Ok(data) = fs::read(path) {
                if let Ok(cached) = serde_json::from_slice::<CachedModule>(&data) {
                    if cached.key == key
                        && cached.shard.fingerprint == fingerprint
                        && cached.shard.file == module.rel_path
                        && cached.shard.module_name == module.module_name
                        && serde_json::to_vec(&cached.shard)
                            .is_ok_and(|data| xxh3_64(&data) == cached.checksum)
                    {
                        self.reused.fetch_add(1, Ordering::Relaxed);
                        return cached.shard;
                    }
                }
            }
        }
        self.parsed.fetch_add(1, Ordering::Relaxed);
        let source = String::from_utf8_lossy(&bytes);
        let shard = index_module_str(
            &module.module_name,
            &module.rel_path,
            module.is_package_init,
            module.is_test_module,
            &source,
            fingerprint,
            ws,
        );
        if shard.outcome == ParseOutcome::Indexed {
            if let Some(path) = path {
                let checksum = xxh3_64(&serde_json::to_vec(&shard).unwrap());
                let cached = CachedModule {
                    key,
                    checksum,
                    shard: shard.clone(),
                };
                if let Some(parent) = path.parent() {
                    if fs::create_dir_all(parent).is_ok() {
                        // A cache is optional: read-only directories and failed writes cannot change analysis.
                        let _ = crate::snapshot::write_file_atomically(
                            &path,
                            &serde_json::to_vec(&cached).unwrap(),
                        );
                    }
                }
            }
        }
        shard
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            reused: self.reused.load(Ordering::Relaxed),
            parsed: self.parsed.load(Ordering::Relaxed),
        }
    }
}

/// Byte offsets from a saved index must never be applied to changed source files.
pub fn verify_source_fingerprints(
    index: &crate::graph::ProgramIndex,
    root: &std::path::Path,
) -> Result<(), String> {
    for module in &index.meta.modules {
        if module.outcome != ParseOutcome::Indexed {
            return Err(format!(
                "Index contains incomplete module '{}'; re-index successfully before rewriting",
                module.file
            ));
        }
        let bytes = fs::read(root.join(&module.file))
            .map_err(|e| format!("Cannot verify indexed source '{}': {e}", module.file))?;
        if xxh3_64(&bytes) != module.fingerprint {
            return Err(format!(
                "Stale index: '{}' changed; rebuild the index before rewriting",
                module.file
            ));
        }
    }
    Ok(())
}
