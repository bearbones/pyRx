use crate::check::{CheckSeverity, Finding};
use crate::graph::ProgramIndex;
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet};

/// Helper to rank candidate canonical functions for `semantic-clone` consolidation.
/// Prefers functions in `utils`/`common`/`shared`/`core`/`helpers` modules, then higher caller count,
/// then lexicographically earliest `qualified_name`.
pub fn select_canonical_entity<'a>(
    candidates: &[&'a EntityNode],
    index: &ProgramIndex,
) -> &'a EntityNode {
    let mut best = candidates[0];
    let mut best_score = canonical_score(best, index);
    for &cand in &candidates[1..] {
        let sc = canonical_score(cand, index);
        if sc > best_score || (sc == best_score && cand.qualified_name < best.qualified_name) {
            best = cand;
            best_score = sc;
        }
    }
    best
}

fn canonical_score(ent: &EntityNode, index: &ProgramIndex) -> (u8, usize) {
    let mod_lower = ent.module.to_ascii_lowercase();
    let is_util_mod = mod_lower.split('.').any(|seg| {
        matches!(
            seg,
            "utils" | "util" | "common" | "shared" | "core" | "helpers" | "lib"
        )
    });
    let caller_count = index.effective_callers(&ent.qualified_name).len();
    (u8::from(is_util_mod), caller_count)
}

pub fn check_semantic_clone(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    check_semantic_clone_with_options(index, severity, 5)
}

pub fn check_semantic_clone_with_options(
    index: &ProgramIndex,
    severity: CheckSeverity,
    min_statements: u32,
) -> Vec<Finding> {
    let min_stmts = min_statements.max(1);
    let mut findings = Vec::new();
    let candidates: Vec<&EntityNode> = index
        .entities
        .iter()
        .filter(|e| {
            matches!(e.kind, EntityKind::Function | EntityKind::AsyncFunction)
                && e.skeleton_hash != 0
                && e.statement_count >= min_stmts
                && !e.name.starts_with("test_")
                && !crate::workspace::is_test_path(&e.file, &e.module)
        })
        .collect();

    // 1. Exact skeleton clones (`skeleton_hash` equality across modules)
    let mut by_hash: BTreeMap<u64, Vec<&EntityNode>> = BTreeMap::new();
    for ent in &candidates {
        by_hash.entry(ent.skeleton_hash).or_default().push(*ent);
    }

    let mut flagged_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut exact_cloned_entities: BTreeSet<String> = BTreeSet::new();

    for group in by_hash.values() {
        let distinct_modules: BTreeSet<&str> = group.iter().map(|e| e.module.as_str()).collect();
        if group.len() < 2 || distinct_modules.len() < 2 {
            continue;
        }
        let canonical = select_canonical_entity(group, index);
        for ent in group {
            exact_cloned_entities.insert(ent.qualified_name.clone());
            if ent.qualified_name == canonical.qualified_name {
                continue;
            }
            flagged_pairs.insert((canonical.qualified_name.clone(), ent.qualified_name.clone()));
            findings.push(Finding::new(
                "semantic-clone",
                "exact_skeleton_clone",
                severity,
                &ent.file,
                ent.line,
                ent.column,
                &ent.qualified_name,
                vec![canonical.qualified_name.clone(), ent.qualified_name.clone()],
                format!(
                    "Function '{}' has an identical normalized AST/CFG skeleton to '{}' ({}:{}); review whether sharing an implementation with '{}' would be appropriate (similarity does not imply equivalent behavior)",
                    ent.qualified_name,
                    canonical.qualified_name,
                    canonical.file,
                    canonical.line,
                    canonical.qualified_name
                ),
                Some(canonical.qualified_name.clone()),
            ));
        }
    }

    // 2. Near-duplicate skeleton clones (Jaccard similarity >= 0.80 on 3-gram feature shingles)
    let near_min = min_stmts.max(3);
    for i in 0..candidates.len() {
        let a = candidates[i];
        if a.statement_count < near_min || a.skeleton_features.len() < 3 {
            continue;
        }
        for b in candidates.iter().skip(i + 1) {
            if b.statement_count < near_min || b.skeleton_features.len() < 3 {
                continue;
            }
            if a.module == b.module || a.skeleton_hash == b.skeleton_hash {
                continue;
            }
            if exact_cloned_entities.contains(&a.qualified_name)
                && exact_cloned_entities.contains(&b.qualified_name)
            {
                continue;
            }
            let sim = jaccard_u64(&a.skeleton_features, &b.skeleton_features);
            if sim >= 0.80 {
                let pair = [a, *b];
                let canonical = select_canonical_entity(&pair, index);
                let dup = if canonical.qualified_name == a.qualified_name {
                    *b
                } else {
                    a
                };
                if flagged_pairs
                    .insert((canonical.qualified_name.clone(), dup.qualified_name.clone()))
                {
                    findings.push(Finding::new(
                        "semantic-clone",
                        "near_skeleton_clone",
                        severity,
                        &dup.file,
                        dup.line,
                        dup.column,
                        &dup.qualified_name,
                        vec![canonical.qualified_name.clone(), dup.qualified_name.clone()],
                        format!(
                            "Function '{}' is a near-duplicate AST/CFG clone ({:.0}% similarity) of '{}' ({}:{}); review whether sharing an implementation with '{}' would be appropriate (similarity does not imply equivalent behavior)",
                            dup.qualified_name,
                            sim * 100.0,
                            canonical.qualified_name,
                            canonical.file,
                            canonical.line,
                            canonical.qualified_name
                        ),
                        Some(canonical.qualified_name.clone()),
                    ));
                }
            }
        }
    }

    findings
}

fn jaccard_u64(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_a: BTreeSet<u64> = a.iter().copied().collect();
    let set_b: BTreeSet<u64> = b.iter().copied().collect();
    let inter = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

pub fn check_schema_drift(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();
    let schemas: Vec<&ClassSchema> = index
        .schemas
        .iter()
        .filter(|s| {
            !s.fields.is_empty()
                && !crate::workspace::is_test_path(&s.file, &s.module)
                && !s.file.starts_with("examples/")
                && !s.file.starts_with("docs/")
        })
        .collect();
    let mut reported_schemas: BTreeSet<String> = BTreeSet::new();

    for i in 0..schemas.len() {
        let a = schemas[i];
        let a_map: BTreeMap<&str, &str> = a
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.type_annotation.as_str()))
            .collect();
        let a_keys: BTreeSet<&str> = a_map.keys().copied().collect();
        let a_stem = domain_entity_stem(&a.name);

        for &b in schemas.iter().skip(i + 1) {
            if a.module == b.module || reported_schemas.contains(&b.qualified_name) {
                continue;
            }
            let b_map: BTreeMap<&str, &str> = b
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.type_annotation.as_str()))
                .collect();
            let b_keys: BTreeSet<&str> = b_map.keys().copied().collect();
            let b_stem = domain_entity_stem(&b.name);

            let shared_keys: Vec<&str> = a_keys.intersection(&b_keys).copied().collect();
            let union_count = a_keys.union(&b_keys).count();
            let overlap = if union_count > 0 {
                shared_keys.len() as f64 / union_count as f64
            } else {
                0.0
            };

            let same_entity_name = a.name == b.name || (!a_stem.is_empty() && a_stem == b_stem);
            if !same_entity_name && (shared_keys.len() < 2 || overlap < 0.60) {
                continue;
            }
            if shared_keys.is_empty() {
                continue;
            }

            // Check for conflicting field types or drifted field sets
            let mut type_conflicts = Vec::new();
            for &k in &shared_keys {
                let ta = a_map[k];
                let tb = b_map[k];
                if ta != tb && !ta.is_empty() && !tb.is_empty() {
                    type_conflicts.push(format!("'{k}' ({ta} vs {tb})"));
                }
            }

            let only_a: Vec<&str> = a_keys.difference(&b_keys).copied().collect();
            let only_b: Vec<&str> = b_keys.difference(&a_keys).copied().collect();

            if !type_conflicts.is_empty()
                || !only_a.is_empty()
                || !only_b.is_empty()
                || same_entity_name
            {
                reported_schemas.insert(b.qualified_name.clone());
                let mut details = Vec::new();
                if !type_conflicts.is_empty() {
                    details.push(format!(
                        "conflicting field types: {}",
                        type_conflicts.join(", ")
                    ));
                }
                if !only_a.is_empty() || !only_b.is_empty() {
                    details.push(format!(
                        "divergent fields (only in '{}': [{}]; only in '{}': [{}])",
                        a.name,
                        only_a.join(", "),
                        b.name,
                        only_b.join(", ")
                    ));
                }
                if details.is_empty() {
                    details.push("duplicate schema definition across modules".to_string());
                }

                findings.push(Finding::new(
                    "schema-drift",
                    "schema_drift",
                    severity,
                    &b.file,
                    b.line,
                    b.column,
                    &b.qualified_name,
                    vec![a.qualified_name.clone(), b.qualified_name.clone()],
                    format!(
                        "Schema '{}' ({:?}) drifts from '{}' ({}:{}: {:?}): {}",
                        b.qualified_name,
                        b.flavor,
                        a.qualified_name,
                        a.file,
                        a.line,
                        a.flavor,
                        details.join("; ")
                    ),
                    Some(a.qualified_name.clone()),
                ));
            }
        }
    }

    findings
}

fn domain_entity_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for suffix in [
        "model", "schema", "dto", "record", "payload", "data", "info", "config", "item",
    ] {
        if let Some(stripped) = lower.strip_suffix(suffix) {
            if stripped.len() >= 3 {
                return stripped.to_string();
            }
        }
    }
    lower
}

pub fn check_dict_contract_mismatch(index: &ProgramIndex, severity: CheckSeverity) -> Vec<Finding> {
    let mut findings = Vec::new();

    for consumer in &index.entities {
        for access in &consumer.dict_accesses {
            if !access.is_required_subscript {
                continue;
            }
            // Case 1: Consumer accesses a key on the dict returned by `source_callee`
            if let Some(prod_qname) = &access.source_callee {
                if let Some(&prod_idx) = index.entity_by_qname.get(prod_qname) {
                    let producer = &index.entities[prod_idx];
                    // Check cross-module boundary (or any producer with known dict returns)
                    if !producer.dict_returns.is_empty() {
                        let key_present_in_any = producer
                            .dict_returns
                            .iter()
                            .any(|shape| shape.keys.iter().any(|k| k == &access.key));
                        if !key_present_in_any {
                            let mut all_prod_keys = BTreeSet::new();
                            for shape in &producer.dict_returns {
                                for k in &shape.keys {
                                    all_prod_keys.insert(k.as_str());
                                }
                            }
                            let prod_keys_list: Vec<&str> = all_prod_keys.into_iter().collect();
                            findings.push(Finding::new(
                                "dict-contract-mismatch",
                                "return_dict_key_mismatch",
                                severity,
                                &consumer.file,
                                access.line,
                                access.column,
                                &consumer.qualified_name,
                                vec![
                                    producer.qualified_name.clone(),
                                    consumer.qualified_name.clone(),
                                    access.key.clone(),
                                ],
                                format!(
                                    "Dict key '{}' accessed in '{}' is not produced by '{}' ({}:{}; produced keys: [{}])",
                                    access.key,
                                    consumer.qualified_name,
                                    producer.qualified_name,
                                    producer.file,
                                    producer.line,
                                    prod_keys_list.join(", ")
                                ),
                                None,
                            ));
                        }
                    }
                }
            }

            // Case 2: Callee (`consumer`) accesses `param["key"]` on its parameter `param_name`,
            // while a caller passes a dict literal missing `"key"`.
            if let Some(param_name) = &access.param_name {
                let param_pos = consumer
                    .parameters
                    .iter()
                    .filter(|p| p.name != "self" && p.name != "cls")
                    .position(|p| &p.name == param_name);
                for edge in index.effective_callers(&consumer.qualified_name) {
                    for (arg_id, passed_keys) in &edge.passed_dict_keys {
                        let matches_param = arg_id == param_name
                            || (param_pos.is_some() && arg_id == &param_pos.unwrap().to_string());
                        if matches_param
                            && !passed_keys.is_empty()
                            && !passed_keys.iter().any(|k| k == &access.key)
                        {
                            findings.push(Finding::new(
                                "dict-contract-mismatch",
                                "passed_dict_key_mismatch",
                                severity,
                                &edge.file,
                                edge.line,
                                edge.column,
                                &edge.caller,
                                vec![
                                    edge.caller.clone(),
                                    consumer.qualified_name.clone(),
                                    access.key.clone(),
                                ],
                                format!(
                                    "Caller '{}' passes dict with keys [{}] for parameter '{}' of '{}', which requires key '{}' ({}:{})",
                                    edge.caller,
                                    passed_keys.join(", "),
                                    param_name,
                                    consumer.qualified_name,
                                    access.key,
                                    consumer.file,
                                    access.line
                                ),
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    findings
}

pub fn check_uncentralized_env_access(
    index: &ProgramIndex,
    severity: CheckSeverity,
    config_modules: &BTreeSet<String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut by_var: BTreeMap<&str, Vec<&EnvAccess>> = BTreeMap::new();
    let mut non_config_accesses: Vec<&EnvAccess> = Vec::new();

    for acc in &index.env_accesses {
        if crate::workspace::is_test_path(&acc.file, &acc.module) {
            continue;
        }
        by_var.entry(acc.var_name.as_str()).or_default().push(acc);
        if !is_config_module(&acc.module, config_modules) {
            non_config_accesses.push(acc);
        }
    }

    // 1. Check inconsistent defaults or type conversions for the same env var across modules
    for (var_name, list) in &by_var {
        let distinct_defaults: BTreeSet<&str> = list
            .iter()
            .filter_map(|a| a.default_value.as_deref())
            .collect();
        let distinct_casts: BTreeSet<&str> = list
            .iter()
            .map(|a| a.cast_type.as_deref().unwrap_or("str"))
            .collect();
        let distinct_mods: BTreeSet<&str> = list.iter().map(|a| a.module.as_str()).collect();

        if distinct_defaults.len() > 1 || (distinct_casts.len() > 1 && distinct_mods.len() > 1) {
            let primary = list[0];
            for dup in list.iter().skip(1) {
                if dup.default_value != primary.default_value || dup.cast_type != primary.cast_type
                {
                    findings.push(Finding::new(
                        "uncentralized-env-access",
                        "inconsistent_env_var",
                        severity,
                        &dup.file,
                        dup.line,
                        dup.column,
                        &dup.enclosing_entity,
                        vec![var_name.to_string(), dup.enclosing_entity.clone()],
                        format!(
                            "Environment variable '{}' has inconsistent default/type at {}:{} (default={:?}, type={:?}) vs {}:{} (default={:?}, type={:?}); centralize environment parsing in a config module (matching `config`, `settings`, `env`, `constants`, `cli`, `__main__`, `conftest`, or `config_modules` in `.pyrx.toml`)",
                            var_name,
                            dup.file,
                            dup.line,
                            dup.default_value,
                            dup.cast_type.as_deref().unwrap_or("str"),
                            primary.file,
                            primary.line,
                            primary.default_value,
                            primary.cast_type.as_deref().unwrap_or("str"),
                        ),
                        None,
                    ));
                }
            }
        }
    }

    // 2. Flag scattered env reads across multiple non-config modules
    let non_config_mods: BTreeSet<&str> = non_config_accesses
        .iter()
        .map(|a| a.module.as_str())
        .collect();
    let has_any_config_mod = index
        .meta
        .modules
        .iter()
        .any(|m| is_config_module(&m.module_name, config_modules));

    if non_config_mods.len() >= 2 || (has_any_config_mod && !non_config_accesses.is_empty()) {
        for acc in non_config_accesses {
            findings.push(Finding::new(
                "uncentralized-env-access",
                "scattered_env_read",
                severity,
                &acc.file,
                acc.line,
                acc.column,
                &acc.enclosing_entity,
                vec![acc.var_name.clone(), acc.enclosing_entity.clone()],
                format!(
                    "Environment variable '{}' read directly in non-config module '{}' (config modules are identified by segments matching `config`, `settings`, `env`, `constants`, `cli`, `__main__`, `conftest`, or patterns in `config_modules` in `.pyrx.toml`); centralize environment configuration",
                    acc.var_name, acc.module
                ),
                None,
            ));
        }
    }

    findings
}

pub fn check_env_sprawl(
    index: &ProgramIndex,
    severity: CheckSeverity,
    config_modules: &BTreeSet<String>,
) -> Vec<Finding> {
    check_uncentralized_env_access(index, severity, config_modules)
}

pub fn is_config_module(module_name: &str, config_modules: &BTreeSet<String>) -> bool {
    let mod_lower = module_name.to_ascii_lowercase();
    if config_modules.contains(&mod_lower) || config_modules.contains(module_name) {
        return true;
    }
    for pat in config_modules {
        let pat_lower = pat.to_ascii_lowercase();
        if let Some(prefix) = pat_lower.strip_suffix('*') {
            if mod_lower.starts_with(prefix) {
                return true;
            }
        } else if let Some(suffix) = pat_lower.strip_prefix('*') {
            if mod_lower.ends_with(suffix) {
                return true;
            }
        }
    }
    for seg in module_name.split('.') {
        let lower = seg.to_ascii_lowercase();
        if config_modules.contains(&lower)
            || lower.contains("config")
            || lower.contains("settings")
            || lower.contains("env")
            || matches!(
                lower.as_str(),
                "constants" | "cli" | "main" | "__main__" | "conftest"
            )
        {
            return true;
        }
    }
    false
}
