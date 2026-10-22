//! Diagnostic evidence is independent of display length and finding fingerprints.
use super::Finding;
use crate::graph::ProgramIndex;
use crate::model::{CallEdge, EdgeConfidence};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceStep {
    pub caller: String,
    pub callee: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub confidence: EdgeConfidence,
}

impl From<&CallEdge> for EvidenceStep {
    fn from(edge: &CallEdge) -> Self {
        Self {
            caller: edge.caller.clone(),
            callee: edge.callee.clone(),
            file: edge.file.clone(),
            line: edge.line,
            column: edge.column,
            confidence: edge.confidence,
        }
    }
}

/// Keep the endpoints and explicitly count omitted frames. All names remain in JSON evidence.
pub fn compact_path(path: &[String]) -> String {
    if path.len() <= 6 {
        return path.join(" -> ");
    }
    format!(
        "{} -> ... ({} frames omitted) -> {}",
        path[..3].join(" -> "),
        path.len() - 6,
        path[path.len() - 3..].join(" -> ")
    )
}

impl Finding {
    pub fn with_evidence(mut self, path: &[CallEdge]) -> Self {
        self.evidence = path.iter().map(EvidenceStep::from).collect();
        self
    }

    /// Resolve the symbol path to call-site evidence. Used for import paths separately below.
    pub fn with_import_path(mut self, index: &ProgramIndex, path: &[String]) -> Self {
        for pair in path.windows(2) {
            if let Some(edge) = index.imports.iter().find(|edge| {
                edge.importer_module == pair[0]
                    && edge.target_module == pair[1]
                    && !edge.is_type_checking
                    && !edge.is_lazy
            }) {
                self.evidence.push(EvidenceStep {
                    caller: pair[0].clone(),
                    callee: pair[1].clone(),
                    file: edge.file.clone(),
                    line: edge.line,
                    column: edge.column,
                    confidence: EdgeConfidence::Proven,
                });
            }
        }
        self
    }

    pub fn full_path(&self) -> Vec<String> {
        let Some(first) = self.evidence.first() else {
            return Vec::new();
        };
        std::iter::once(first.caller.clone())
            .chain(self.evidence.iter().map(|e| e.callee.clone()))
            .collect()
    }
}

pub fn call_path(edges: &[CallEdge]) -> Vec<String> {
    let Some(first) = edges.first() else {
        return Vec::new();
    };
    std::iter::once(first.caller.clone())
        .chain(edges.iter().map(|e| e.callee.clone()))
        .collect()
}

/// Reconstruct a BFS witness without storing a full path on every queued node.
pub fn forward_path(
    parents: &std::collections::BTreeMap<String, CallEdge>,
    end: &str,
) -> Vec<CallEdge> {
    let mut path = Vec::new();
    let mut current = end;
    while let Some(edge) = parents.get(current) {
        path.push(edge.clone());
        current = &edge.caller;
    }
    path.reverse();
    path
}
