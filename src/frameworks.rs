//! Narrow framework entry-point adapters. No framework code is imported or executed.
use crate::model::{EntityKind, ModuleShard};
use crate::workspace::WorkspaceConfig;
use std::collections::BTreeSet;

pub fn mark_entry_points(shards: &mut [ModuleShard], workspace: &WorkspaceConfig) {
    if !workspace.framework_entry_points {
        return;
    }
    let mut framework_classes = BTreeSet::new();
    // Include local subclasses of known Home Assistant framework bases.
    loop {
        let mut added = false;
        for shard in shards.iter() {
            for entity in &shard.entities {
                if entity.kind == EntityKind::Class
                    && entity.bases.iter().any(|base| {
                        (base.starts_with("homeassistant.")
                            && (base.ends_with("Entity")
                                || base.ends_with("ConfigFlow")
                                || base.ends_with("OptionsFlow")))
                            || framework_classes.contains(base)
                    })
                {
                    added |= framework_classes.insert(entity.qualified_name.clone());
                }
            }
        }
        if !added {
            break;
        }
    }
    for shard in shards {
        let parts: Vec<_> = shard.file.split('/').collect();
        let custom_integration = parts.windows(2).any(|pair| {
            pair[0] == "custom_components"
                && workspace
                    .project_root
                    .join("custom_components")
                    .join(pair[1])
                    .join("manifest.json")
                    .is_file()
        });
        for entity in &mut shard.entities {
            let pytest = entity
                .decorators
                .iter()
                .any(|dec| matches!(dec.as_str(), "pytest.fixture" | "pytest_asyncio.fixture"));
            let lifecycle = custom_integration
                && entity.enclosing_class.is_none()
                && matches!(
                    entity.name.as_str(),
                    "setup"
                        | "async_setup"
                        | "setup_entry"
                        | "async_setup_entry"
                        | "async_unload_entry"
                        | "async_remove_entry"
                        | "async_migrate_entry"
                        | "setup_platform"
                        | "async_setup_platform"
                );
            let registered_callback = entity
                .decorators
                .iter()
                .any(|dec| dec == "homeassistant.components.websocket_api.websocket_command");
            let framework_member = framework_classes.contains(&entity.qualified_name)
                || entity.enclosing_class.as_ref().is_some_and(|cls| {
                    framework_classes.contains(cls) && !entity.name.starts_with('_')
                });
            if pytest || lifecycle || registered_callback || framework_member {
                entity.is_entry_point = true;
                entity.entry_point_kind = Some(
                    if pytest {
                        "framework:pytest"
                    } else {
                        "framework:homeassistant"
                    }
                    .to_string(),
                );
            }
        }
    }
}
