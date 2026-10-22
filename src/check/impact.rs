//! Keep findings whose causes can be affected by a patch, even when their primary location is unchanged.
use super::Finding;
use crate::graph::ProgramIndex;
use crate::model::EntityKind;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Default)]
pub struct PatchImpact {
    symbols: BTreeSet<String>,
    modules: BTreeSet<String>,
}

impl PatchImpact {
    pub fn new(index: &ProgramIndex, ranges: &BTreeMap<String, Vec<(u32, u32)>>) -> Self {
        let mut impact = Self::default();
        for entity in &index.entities {
            if let Some(lines) = ranges.get(&entity.file) {
                impact.modules.insert(entity.module.clone());
                if entity.kind != EntityKind::Module
                    && lines.iter().any(|&(start, end)| {
                        entity.line <= end.max(start) && entity.end_line >= start
                    })
                {
                    impact.symbols.insert(entity.qualified_name.clone());
                }
            }
        }
        let mut queue: VecDeque<_> = impact.symbols.iter().cloned().collect();
        while let Some(symbol) = queue.pop_front() {
            for edge in index.effective_callers(&symbol) {
                if impact.symbols.insert(edge.caller.clone()) {
                    queue.push_back(edge.caller);
                }
            }
        }
        let mut queue: VecDeque<_> = impact.modules.iter().cloned().collect();
        while let Some(module) = queue.pop_front() {
            for edge in &index.imports {
                if edge.target_module == module
                    && !edge.is_type_checking
                    && !edge.is_lazy
                    && impact.modules.insert(edge.importer_module.clone())
                {
                    queue.push_back(edge.importer_module.clone());
                }
            }
        }
        impact
    }

    pub fn contains(&self, finding: &Finding, index: &ProgramIndex) -> bool {
        if self.symbols.contains(&finding.scope)
            || finding.entities.iter().any(|s| self.symbols.contains(s))
        {
            return true;
        }
        // Import diagnostics depend on module initialization, not just callable bodies.
        if matches!(
            finding.check.as_str(),
            "import-cycle" | "import-side-effects" | "heavy-import-leak"
        ) {
            return index.meta.modules.iter().any(|module| {
                module.file == finding.file && self.modules.contains(&module.module_name)
            });
        }
        false
    }
}
