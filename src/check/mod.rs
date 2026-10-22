pub mod async_conc;
pub mod evidence;
pub mod exceptions;
mod impact;
pub mod imports;
pub mod reachability;
pub mod sprawl;

use crate::diff::parse_unified_diff;
use crate::graph::ProgramIndex;
use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSeverity {
    Note,
    Warning,
    Error,
}

impl CheckSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn sarif_level(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FailOnThreshold {
    Note,
    Warning,
    Error,
    None,
}

impl FailOnThreshold {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "note" => Ok(Self::Note),
            "warning" | "warn" => Ok(Self::Warning),
            "error" | "err" => Ok(Self::Error),
            "none" | "off" => Ok(Self::None),
            other => Err(format!(
                "Invalid --fail-on value '{other}' (expected note|warning|error|none)"
            )),
        }
    }

    pub fn triggers(self, sev: CheckSeverity) -> bool {
        match self {
            Self::None => false,
            Self::Note => true,
            Self::Warning => matches!(sev, CheckSeverity::Warning | CheckSeverity::Error),
            Self::Error => matches!(sev, CheckSeverity::Error),
        }
    }
}

pub struct CheckDescriptor {
    pub name: &'static str,
    pub category: &'static str,
    pub default_enabled: bool,
    pub default_severity: CheckSeverity,
    pub description: &'static str,
}

pub const ALL_CHECKS: &[CheckDescriptor] = &[
    // Category A: Anti-Sprawl & LLM Consolidation Checks
    CheckDescriptor {
        name: "semantic-clone",
        category: "sprawl",
        default_enabled: true,
        default_severity: CheckSeverity::Note,
        description: "Functions/methods across modules with identical or near-identical normalized AST/CFG skeletons",
    },
    CheckDescriptor {
        name: "schema-drift",
        category: "sprawl",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Multiple dataclass, NamedTuple, TypedDict, or Pydantic models with overlapping or conflicting fields",
    },
    CheckDescriptor {
        name: "dict-contract-mismatch",
        category: "sprawl",
        default_enabled: true,
        default_severity: CheckSeverity::Error,
        description: "Producer/consumer dict literal keys disagree with keys accessed across module boundaries",
    },
    CheckDescriptor {
        name: "uncentralized-env-access",
        category: "sprawl",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Scattered os.environ/os.getenv reads across non-config modules or inconsistent defaults/types",
    },
    // Category B: Whole-Program Reachability & Dead Subgraph Checks
    CheckDescriptor {
        name: "dead-code",
        category: "reachability",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Functions, methods, classes, and utility modules unreachable from any entry point",
    },
    CheckDescriptor {
        name: "unused-parameter",
        category: "reachability",
        default_enabled: false,
        default_severity: CheckSeverity::Note,
        description: "Parameters on internal functions never read or passed the same constant at every call site",
    },
    CheckDescriptor {
        name: "test-only-production-code",
        category: "reachability",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Production symbols whose only callers in the entire repository are under tests/",
    },
    // Category C: Interprocedural Exception & Error-Boundary Checks
    CheckDescriptor {
        name: "uncaught-exception-escape",
        category: "exceptions",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Transitive raise paths escaping a declared entry point without a matching except handler",
    },
    CheckDescriptor {
        name: "broad-catch-swallow",
        category: "exceptions",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Broad except blocks that swallow exceptions or fail to re-raise asyncio.CancelledError",
    },
    CheckDescriptor {
        name: "exception-contract-divergence",
        category: "exceptions",
        default_enabled: true,
        default_severity: CheckSeverity::Error,
        description: "Protocol/ABC method overrides that raise exception types not raised by base or sibling implementations",
    },
    // Category D: Async, Concurrency & Resource Discipline Checks
    CheckDescriptor {
        name: "blocking-in-async",
        category: "async",
        default_enabled: true,
        default_severity: CheckSeverity::Error,
        description: "async def coroutines that transitively call synchronous blocking operations",
    },
    CheckDescriptor {
        name: "lock-order-inversion",
        category: "async",
        default_enabled: true,
        default_severity: CheckSeverity::Error,
        description: "Interprocedural lock acquisition cycles across threading/asyncio locks or await under threading.Lock",
    },
    CheckDescriptor {
        name: "unowned-resource-return",
        category: "async",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Functions that allocate a context-managed resource and return it without caller with/ExitStack ownership",
    },
    // Category E: Import Graph & Module Initialization Checks
    CheckDescriptor {
        name: "import-side-effects",
        category: "imports",
        default_enabled: true,
        default_severity: CheckSeverity::Warning,
        description: "Module top-level code that transitively performs I/O, subprocesses, or global env mutation at import time",
    },
    CheckDescriptor {
        name: "import-cycle",
        category: "imports",
        default_enabled: true,
        default_severity: CheckSeverity::Error,
        description: "Strongly connected components (SCCs) in the runtime module import graph",
    },
    CheckDescriptor {
        name: "heavy-import-leak",
        category: "imports",
        default_enabled: false,
        default_severity: CheckSeverity::Warning,
        description: "Entry points or lightweight modules that transitively import heavyweight packages at module load time",
    },
];

/// Resolves legacy check aliases (`unused-parameter-cross-tu` -> `unused-parameter`,
/// `env-sprawl` -> `uncentralized-env-access`) to their canonical check names.
pub fn canonical_check_name(name: &str) -> &str {
    match name.trim() {
        "unused-parameter-cross-tu" => "unused-parameter",
        "env-sprawl" => "uncentralized-env-access",
        other => other,
    }
}

pub fn legacy_check_alias(canonical: &str) -> Option<&'static str> {
    match canonical {
        "unused-parameter" => Some("unused-parameter-cross-tu"),
        "uncentralized-env-access" => Some("env-sprawl"),
        _ => None,
    }
}

pub fn find_check_descriptor(name: &str) -> Option<&'static CheckDescriptor> {
    let canonical = canonical_check_name(name);
    ALL_CHECKS.iter().find(|c| c.name == canonical)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PerCheckRuleConfig {
    pub enabled: Option<bool>,
    pub severity: Option<CheckSeverity>,
    pub fail: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PyrexCheckConfigFile {
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub disable: Vec<String>,
    #[serde(default)]
    pub fail_on: Option<String>,
    #[serde(default)]
    pub severity_overrides: BTreeMap<String, CheckSeverity>,
    #[serde(default)]
    pub fail_overrides: BTreeMap<String, bool>,
    #[serde(default)]
    pub rules: BTreeMap<String, PerCheckRuleConfig>,
    #[serde(default)]
    pub check_options: BTreeMap<String, Value>,
    #[serde(default)]
    pub entry_points: Vec<String>,
    #[serde(default)]
    pub heavy_modules: Vec<String>,
    #[serde(default)]
    pub config_modules: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedCheckConfig {
    /// Opt in to legacy changed-line-only filtering. Default includes cross-module impact.
    pub diff_lines_only: bool,
    pub enabled_checks: BTreeSet<String>,
    pub severities: BTreeMap<String, CheckSeverity>,
    pub fail_on: FailOnThreshold,
    pub fail_overrides: BTreeMap<String, bool>,
    pub options: BTreeMap<String, String>,
    pub entry_points: Vec<String>,
    pub heavy_modules: BTreeSet<String>,
    pub config_modules: BTreeSet<String>,
}

impl Default for ResolvedCheckConfig {
    fn default() -> Self {
        let mut enabled = BTreeSet::new();
        let mut severities = BTreeMap::new();
        for desc in ALL_CHECKS {
            if desc.default_enabled {
                enabled.insert(desc.name.to_string());
            }
            severities.insert(desc.name.to_string(), desc.default_severity);
        }
        let heavy_modules = [
            "torch",
            "tensorflow",
            "pandas",
            "scipy",
            "grpc",
            "boto3",
            "sqlalchemy",
            "matplotlib",
            "transformers",
            "sklearn",
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect();
        let config_modules = [
            "config",
            "settings",
            "env",
            "constants",
            "cli",
            "main",
            "__main__",
            "conftest",
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect();
        let mut options = BTreeMap::new();
        options.insert(
            "uncaught-exception-escape.check_main".to_string(),
            "false".to_string(),
        );
        options.insert(
            "unused-parameter.check_dunders".to_string(),
            "false".to_string(),
        );
        options.insert(
            "unused-parameter.check_constant_args".to_string(),
            "true".to_string(),
        );
        options.insert("semantic-clone.min_statements".to_string(), "5".to_string());
        Self {
            diff_lines_only: false,
            enabled_checks: enabled,
            severities,
            fail_on: FailOnThreshold::Warning,
            fail_overrides: BTreeMap::new(),
            options,
            entry_points: Vec::new(),
            heavy_modules,
            config_modules,
        }
    }
}

impl ResolvedCheckConfig {
    pub fn load_and_resolve(
        start_dir: &Path,
        explicit_config: Option<&Path>,
        checks_spec: Option<&str>,
    ) -> Result<Self, String> {
        let mut cfg = Self::default();

        let config_path = if let Some(p) = explicit_config {
            if !p.is_file() {
                return Err(format!(
                    "Checks configuration file not found: '{}'",
                    p.display()
                ));
            }
            Some(p.to_path_buf())
        } else {
            discover_check_config_file(start_dir)
        };

        if let Some(p) = config_path {
            let content = fs::read_to_string(&p)
                .map_err(|e| format!("Failed to read '{}': {e}", p.display()))?;
            let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if fname.ends_with(".toml") {
                cfg.apply_toml_config(&content, &p, fname == "pyproject.toml")?;
            } else {
                let parsed: PyrexCheckConfigFile = serde_json::from_str(&content)
                    .map_err(|e| format!("Invalid JSON in '{}': {e}", p.display()))?;
                cfg.apply_parsed_config(parsed)?;
            }
        }

        if let Some(spec) = checks_spec {
            cfg.apply_spec_tokens(spec.split(','))?;
        }

        cfg.entry_points.sort();
        cfg.entry_points.dedup();
        Ok(cfg)
    }

    pub fn load_and_resolve_with_options(
        start_dir: &Path,
        explicit_config: Option<&Path>,
        checks_spec: Option<&str>,
        cli_options: &[String],
    ) -> Result<Self, String> {
        let mut cfg = Self::load_and_resolve(start_dir, explicit_config, checks_spec)?;
        for kv in cli_options {
            cfg.apply_check_option_kv(kv)?;
        }
        Ok(cfg)
    }

    pub fn set_option(&mut self, check: &str, key: &str, val: &str) -> Result<(), String> {
        let Some(desc) = find_check_descriptor(check) else {
            return Err(format!(
                "Unknown check name '{check}' in check option '{check}.{key}'"
            ));
        };
        let canonical = desc.name;
        let full_key = format!("{canonical}.{key}");
        self.options
            .insert(full_key.clone(), val.trim().to_string());
        if canonical == "heavy-import-leak" && key == "heavy_modules" {
            self.heavy_modules = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if canonical == "uncentralized-env-access" && key == "allowed_modules" {
            self.config_modules = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        Ok(())
    }

    pub fn apply_check_option_kv(&mut self, kv: &str) -> Result<(), String> {
        let Some((lhs, rhs)) = kv.split_once('=') else {
            return Err(format!(
                "Invalid check option '{kv}' (expected <check>.<key>=<value>)"
            ));
        };
        let Some((check, key)) = lhs.trim().rsplit_once('.') else {
            return Err(format!(
                "Invalid check option key '{}' (expected <check>.<key>=<value>)",
                lhs.trim()
            ));
        };
        self.set_option(check.trim(), key.trim(), rhs.trim())
    }

    pub fn option_bool(&self, check: &str, key: &str, default: bool) -> bool {
        let canonical = canonical_check_name(check);
        let full_key = format!("{canonical}.{key}");
        let Some(val) = self.options.get(&full_key) else {
            return default;
        };
        match val.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => default,
        }
    }

    pub fn option_u32(&self, check: &str, key: &str, default: u32) -> u32 {
        let canonical = canonical_check_name(check);
        let full_key = format!("{canonical}.{key}");
        self.options
            .get(&full_key)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(default)
    }

    fn apply_parsed_config(&mut self, parsed: PyrexCheckConfigFile) -> Result<(), String> {
        if !parsed.checks.is_empty() {
            self.apply_spec_tokens(parsed.checks.iter().map(|s| s.as_str()))?;
        }
        for dis in &parsed.disable {
            self.apply_spec_token(&format!("-{dis}"))?;
        }
        if let Some(fo) = &parsed.fail_on {
            self.fail_on = FailOnThreshold::parse(fo)?;
        }
        for (k, v) in parsed.severity_overrides {
            let Some(desc) = find_check_descriptor(&k) else {
                return Err(format!("Unknown check name '{k}' in severity_overrides"));
            };
            self.severities.insert(desc.name.to_string(), v);
        }
        for (k, v) in parsed.fail_overrides {
            let Some(desc) = find_check_descriptor(&k) else {
                return Err(format!("Unknown check name '{k}' in fail_overrides"));
            };
            self.fail_overrides.insert(desc.name.to_string(), v);
        }
        for (k, rule) in parsed.rules {
            let Some(desc) = find_check_descriptor(&k) else {
                return Err(format!("Unknown check name '{k}' in rules"));
            };
            if let Some(en) = rule.enabled {
                if en {
                    self.enabled_checks.insert(desc.name.to_string());
                } else {
                    self.enabled_checks.remove(desc.name);
                }
            }
            if let Some(sev) = rule.severity {
                self.severities.insert(desc.name.to_string(), sev);
            }
            if let Some(f) = rule.fail {
                self.fail_overrides.insert(desc.name.to_string(), f);
            }
        }
        for (k, v) in parsed.check_options {
            if let Some(obj) = v.as_object() {
                for (sub_k, sub_v) in obj {
                    self.set_option(&k, sub_k, &json_val_to_option_str(sub_v))?;
                }
            } else if let Some((chk, opt_k)) = k.rsplit_once('.') {
                self.set_option(chk, opt_k, &json_val_to_option_str(&v))?;
            }
        }
        for ep in parsed.entry_points {
            self.entry_points.push(ep);
        }
        for hm in parsed.heavy_modules {
            self.heavy_modules.insert(hm);
        }
        for cm in parsed.config_modules {
            self.config_modules.insert(cm);
        }
        Ok(())
    }

    fn apply_toml_config(
        &mut self,
        content: &str,
        path: &Path,
        is_pyproject: bool,
    ) -> Result<(), String> {
        let root_val: toml::Value = content
            .parse()
            .map_err(|e| format!("Invalid TOML in '{}': {e}", path.display()))?;
        let Some(root_tbl) = root_val.as_table() else {
            return Ok(());
        };
        let target_tbl = if is_pyproject {
            let Some(tool) = root_tbl.get("tool").and_then(|v| v.as_table()) else {
                return Ok(());
            };
            if let Some(t) = tool.get("pyrx").and_then(|v| v.as_table()) {
                t
            } else if let Some(t) = tool.get("pyrex").and_then(|v| v.as_table()) {
                t
            } else {
                return Ok(());
            }
        } else {
            root_tbl
        };

        if let Some(arr) = target_tbl.get("checks").and_then(|v| v.as_array()) {
            self.apply_spec_tokens(arr.iter().filter_map(|v| v.as_str()))?;
        }
        if let Some(arr) = target_tbl.get("disable").and_then(|v| v.as_array()) {
            for dis in arr.iter().filter_map(|v| v.as_str()) {
                self.apply_spec_token(&format!("-{dis}"))?;
            }
        }
        if let Some(fo) = target_tbl.get("fail_on").and_then(|v| v.as_str()) {
            self.fail_on = FailOnThreshold::parse(fo)?;
        }
        if let Some(sev_tbl) = target_tbl
            .get("severity_overrides")
            .and_then(|v| v.as_table())
        {
            for (k, v) in sev_tbl {
                let Some(desc) = find_check_descriptor(k) else {
                    return Err(format!("Unknown check name '{k}' in severity_overrides"));
                };
                if let Some(s_str) = v.as_str() {
                    let sev = parse_severity_str(s_str)?;
                    self.severities.insert(desc.name.to_string(), sev);
                }
            }
        }
        if let Some(fail_tbl) = target_tbl.get("fail_overrides").and_then(|v| v.as_table()) {
            for (k, v) in fail_tbl {
                let Some(desc) = find_check_descriptor(k) else {
                    return Err(format!("Unknown check name '{k}' in fail_overrides"));
                };
                if let Some(b) = v.as_bool() {
                    self.fail_overrides.insert(desc.name.to_string(), b);
                }
            }
        }
        for section_name in ["rules", "check"] {
            if let Some(rules_tbl) = target_tbl.get(section_name).and_then(|v| v.as_table()) {
                for (k, v) in rules_tbl {
                    let Some(desc) = find_check_descriptor(k) else {
                        return Err(format!("Unknown check name '{k}' in [{section_name}]"));
                    };
                    if let Some(rt) = v.as_table() {
                        for (rk, rv) in rt {
                            match rk.as_str() {
                                "enabled" => {
                                    if let Some(en) = rv.as_bool() {
                                        if en {
                                            self.enabled_checks.insert(desc.name.to_string());
                                        } else {
                                            self.enabled_checks.remove(desc.name);
                                        }
                                    }
                                }
                                "severity" => {
                                    if let Some(s_str) = rv.as_str() {
                                        let sev = parse_severity_str(s_str)?;
                                        self.severities.insert(desc.name.to_string(), sev);
                                    }
                                }
                                "fail" => {
                                    if let Some(f) = rv.as_bool() {
                                        self.fail_overrides.insert(desc.name.to_string(), f);
                                    }
                                }
                                other_opt => {
                                    self.set_option(
                                        desc.name,
                                        other_opt,
                                        &toml_val_to_option_str(rv),
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(opts_tbl) = target_tbl.get("check_options").and_then(|v| v.as_table()) {
            for (k, v) in opts_tbl {
                if let Some(nested) = v.as_table() {
                    for (sub_k, sub_v) in nested {
                        self.set_option(k, sub_k, &toml_val_to_option_str(sub_v))?;
                    }
                } else if let Some((chk, opt_k)) = k.rsplit_once('.') {
                    self.set_option(chk, opt_k, &toml_val_to_option_str(v))?;
                } else {
                    return Err(format!(
                        "Invalid key '{k}' in [check_options] (expected '<check>.<key>' or '[check_options.<check>]')"
                    ));
                }
            }
        }
        if let Some(ep_val) = target_tbl.get("entry_points") {
            if let Some(arr) = ep_val.as_array() {
                for item in arr.iter().filter_map(|v| v.as_str()) {
                    self.entry_points.push(item.to_string());
                }
            } else if let Some(ep_tbl) = ep_val.as_table() {
                for (_k, v) in ep_tbl {
                    if let Some(s) = v.as_str() {
                        self.entry_points.push(s.to_string());
                    } else if let Some(arr) = v.as_array() {
                        for item in arr.iter().filter_map(|x| x.as_str()) {
                            self.entry_points.push(item.to_string());
                        }
                    }
                }
            }
        }
        if let Some(arr) = target_tbl.get("heavy_modules").and_then(|v| v.as_array()) {
            for hm in arr.iter().filter_map(|v| v.as_str()) {
                self.heavy_modules.insert(hm.to_string());
            }
        }
        if let Some(arr) = target_tbl.get("config_modules").and_then(|v| v.as_array()) {
            for cm in arr.iter().filter_map(|v| v.as_str()) {
                self.config_modules.insert(cm.to_string());
            }
        }
        Ok(())
    }

    pub fn should_fail_finding(
        &self,
        finding: &Finding,
        cli_fail_on: Option<FailOnThreshold>,
    ) -> bool {
        let threshold = cli_fail_on.unwrap_or(self.fail_on);
        if threshold == FailOnThreshold::None {
            return false;
        }
        if let Some(&fail_flag) = self.fail_overrides.get(&finding.check) {
            return fail_flag;
        }
        threshold.triggers(finding.severity)
    }

    pub fn apply_spec_tokens<'a, I>(&mut self, raw_tokens: I) -> Result<(), String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let tokens: Vec<&str> = raw_tokens
            .into_iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(&first) = tokens.first() {
            if !first.starts_with('+') && !first.starts_with('-') && first != "all" && first != "*"
            {
                self.enabled_checks.clear();
            }
        }
        for token in tokens {
            self.apply_spec_token(token)?;
        }
        Ok(())
    }

    pub fn apply_spec_token(&mut self, token: &str) -> Result<(), String> {
        let (enable, name) = if let Some(rest) = token.strip_prefix('-') {
            (false, rest.trim())
        } else if let Some(rest) = token.strip_prefix('+') {
            (true, rest.trim())
        } else {
            (true, token.trim())
        };

        if name == "all" || name == "*" {
            if enable {
                for c in ALL_CHECKS {
                    self.enabled_checks.insert(c.name.to_string());
                }
            } else {
                self.enabled_checks.clear();
            }
            return Ok(());
        }

        // Check category group name (`sprawl`, `reachability`, `exceptions`, `async`, `imports`)
        let group_matches: Vec<&CheckDescriptor> =
            ALL_CHECKS.iter().filter(|c| c.category == name).collect();
        if !group_matches.is_empty() {
            for c in group_matches {
                if enable {
                    self.enabled_checks.insert(c.name.to_string());
                } else {
                    self.enabled_checks.remove(c.name);
                }
            }
            return Ok(());
        }

        if let Some(desc) = find_check_descriptor(name) {
            if enable {
                self.enabled_checks.insert(desc.name.to_string());
            } else {
                self.enabled_checks.remove(desc.name);
            }
            return Ok(());
        }

        Err(format!("Unknown check or check group '{name}'"))
    }
}

fn toml_val_to_option_str(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Array(arr) => arr
            .iter()
            .map(toml_val_to_option_str)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

fn json_val_to_option_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(arr) => arr
            .iter()
            .map(json_val_to_option_str)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

fn parse_severity_str(s: &str) -> Result<CheckSeverity, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "note" | "info" => Ok(CheckSeverity::Note),
        "warning" | "warn" => Ok(CheckSeverity::Warning),
        "error" | "err" => Ok(CheckSeverity::Error),
        other => Err(format!(
            "Invalid check severity '{other}' (expected note|warning|error)"
        )),
    }
}

fn discover_check_config_file(start: &Path) -> Option<PathBuf> {
    let init = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    let mut cur: Option<&Path> = Some(init);
    while let Some(dir) = cur {
        for name in [
            ".pyrx.toml",
            ".pyrex.toml",
            ".pyrx-check.json",
            ".pyrex-check.json",
        ] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.is_file() {
            if let Ok(content) = fs::read_to_string(&pyproject) {
                if content.contains("[tool.pyrx") || content.contains("[tool.pyrex") {
                    return Some(pyproject);
                }
            }
        }
        cur = dir.parent();
    }
    None
}

/// A single cross-module finding emitted by `pyrex check`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub check: String,
    pub kind: String,
    pub severity: CheckSeverity,
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub scope: String,
    pub entities: Vec<String>,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<evidence::EvidenceStep>,
}

impl Finding {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        check: &str,
        kind: &str,
        severity: CheckSeverity,
        file: impl Into<String>,
        line: u32,
        column: u32,
        scope: impl Into<String>,
        entities: Vec<String>,
        message: impl Into<String>,
        canonical_target: Option<String>,
    ) -> Self {
        let file_s = file.into();
        let scope_s = scope.into();
        let msg_s = message.into();
        let fingerprint = compute_finding_fingerprint(check, kind, &file_s, &scope_s, &entities);
        Self {
            check: check.to_string(),
            kind: kind.to_string(),
            severity,
            file: file_s,
            line: line.max(1),
            column: column.max(1),
            message: msg_s,
            scope: scope_s,
            entities,
            fingerprint,
            canonical_target,
            evidence: Vec::new(),
        }
    }
}

/// Computes `pyrexFingerprint/v1` (line-number independent, deterministic `xxh3_64` over check, kind, file, scope, entities).
pub fn compute_finding_fingerprint(
    check: &str,
    kind: &str,
    file: &str,
    scope: &str,
    entities: &[String],
) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"pyrexFingerprint/v1\0");
    buf.extend_from_slice(check.as_bytes());
    buf.push(0);
    buf.extend_from_slice(kind.as_bytes());
    buf.push(0);
    buf.extend_from_slice(file.as_bytes());
    buf.push(0);
    buf.extend_from_slice(scope.as_bytes());
    buf.push(0);
    for ent in entities {
        buf.extend_from_slice(ent.as_bytes());
        buf.push(0);
    }
    let h = xxh3_64(&buf);
    format!("{h:016x}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub fingerprint: String,
    pub count: usize,
    pub check: String,
    pub file: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineFile {
    pub version: u32,
    pub entries: Vec<BaselineEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRunReport {
    pub analysis: AnalysisCoverage,
    pub summary: CheckSummary,
    pub modules: Vec<ModuleStatusRow>,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_baseline: Vec<BaselineEntry>,
}

/// Coverage describes the indexed graph, not a proof about Python's dynamic runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisCoverage {
    pub traversal: String,
    pub call_edges: usize,
    pub internal_call_edges: usize,
    pub external_or_unresolved_call_edges: usize,
    pub uncertain_call_edges: usize,
    pub entry_points: usize,
    pub limitation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSummary {
    pub modules: usize,
    pub analyzed: usize,
    pub failed: usize,
    pub findings: usize,
    pub suppressed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baselined: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_baseline: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_changes: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleStatusRow {
    pub file: String,
    pub module: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Runs all enabled checks in `cfg` over `index` and applies suppressions, baselines, and diff filters.
pub fn run_checks(
    index: &ProgramIndex,
    cfg: &ResolvedCheckConfig,
    baseline: Option<&BaselineFile>,
    patch_text: Option<&str>,
) -> CheckRunReport {
    let mut raw_findings = Vec::new();

    // Category A: Sprawl & Consolidation
    if cfg.enabled_checks.contains("semantic-clone") {
        let sev = cfg.severities["semantic-clone"];
        let min_stmts = cfg.option_u32("semantic-clone", "min_statements", 5);
        raw_findings.extend(sprawl::check_semantic_clone_with_options(
            index, sev, min_stmts,
        ));
    }
    if cfg.enabled_checks.contains("schema-drift") {
        let sev = cfg.severities["schema-drift"];
        raw_findings.extend(sprawl::check_schema_drift(index, sev));
    }
    if cfg.enabled_checks.contains("dict-contract-mismatch") {
        let sev = cfg.severities["dict-contract-mismatch"];
        raw_findings.extend(sprawl::check_dict_contract_mismatch(index, sev));
    }
    if cfg.enabled_checks.contains("uncentralized-env-access") {
        let sev = cfg.severities["uncentralized-env-access"];
        raw_findings.extend(sprawl::check_uncentralized_env_access(
            index,
            sev,
            &cfg.config_modules,
        ));
    }

    // Category B: Reachability & Dead Subgraphs
    if cfg.enabled_checks.contains("dead-code") {
        let sev = cfg.severities["dead-code"];
        raw_findings.extend(reachability::check_dead_code_with_entry_points(
            index,
            sev,
            &cfg.entry_points,
        ));
    }
    if cfg.enabled_checks.contains("unused-parameter") {
        let sev = cfg.severities["unused-parameter"];
        let check_dunders = cfg.option_bool("unused-parameter", "check_dunders", false);
        let check_constant_args = cfg.option_bool("unused-parameter", "check_constant_args", true);
        raw_findings.extend(reachability::check_unused_parameter_with_options(
            index,
            sev,
            check_dunders,
            check_constant_args,
        ));
    }
    if cfg.enabled_checks.contains("test-only-production-code") {
        let sev = cfg.severities["test-only-production-code"];
        raw_findings.extend(
            reachability::check_test_only_production_code_with_entry_points(
                index,
                sev,
                &cfg.entry_points,
            ),
        );
    }

    // Category C: Interprocedural Exception & Error-Boundary
    if cfg.enabled_checks.contains("uncaught-exception-escape") {
        let sev = cfg.severities["uncaught-exception-escape"];
        let check_main = cfg.option_bool("uncaught-exception-escape", "check_main", false);
        raw_findings.extend(exceptions::check_uncaught_exception_escape_with_options(
            index, sev, check_main,
        ));
    }
    if cfg.enabled_checks.contains("broad-catch-swallow") {
        let sev = cfg.severities["broad-catch-swallow"];
        raw_findings.extend(exceptions::check_broad_catch_swallow(index, sev));
    }
    if cfg.enabled_checks.contains("exception-contract-divergence") {
        let sev = cfg.severities["exception-contract-divergence"];
        raw_findings.extend(exceptions::check_exception_contract_divergence(index, sev));
    }

    // Category D: Async, Concurrency & Resource Discipline
    if cfg.enabled_checks.contains("blocking-in-async") {
        let sev = cfg.severities["blocking-in-async"];
        raw_findings.extend(async_conc::check_blocking_in_async(index, sev));
    }
    if cfg.enabled_checks.contains("lock-order-inversion") {
        let sev = cfg.severities["lock-order-inversion"];
        raw_findings.extend(async_conc::check_lock_order_inversion(index, sev));
    }
    if cfg.enabled_checks.contains("unowned-resource-return") {
        let sev = cfg.severities["unowned-resource-return"];
        raw_findings.extend(async_conc::check_unowned_resource_return(index, sev));
    }

    // Category E: Import Graph & Module Initialization
    if cfg.enabled_checks.contains("import-side-effects") {
        let sev = cfg.severities["import-side-effects"];
        raw_findings.extend(imports::check_import_side_effects(index, sev));
    }
    if cfg.enabled_checks.contains("import-cycle") {
        let sev = cfg.severities["import-cycle"];
        raw_findings.extend(imports::check_import_cycle(index, sev));
    }
    if cfg.enabled_checks.contains("heavy-import-leak") {
        let sev = cfg.severities["heavy-import-leak"];
        raw_findings.extend(imports::check_heavy_import_leak(
            index,
            sev,
            &cfg.heavy_modules,
        ));
    }

    // Sort deterministically by (file, line, column, check, fingerprint, message)
    raw_findings.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.check.cmp(&b.check))
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
            .then_with(|| a.message.cmp(&b.message))
    });
    raw_findings.dedup_by(|a, b| {
        a.file == b.file
            && a.line == b.line
            && a.column == b.column
            && a.check == b.check
            && a.fingerprint == b.fingerprint
    });

    // 1. Filter inline suppressions (`# pyrx: ignore[check]` or `# pyrex: ignore[check]`)
    let mut suppressed_count = 0usize;
    let mut unsuppressed = Vec::new();
    for f in raw_findings {
        let suppressed_canonical = index.is_suppressed(&f.file, f.line, &f.check);
        let suppressed_legacy = legacy_check_alias(&f.check)
            .is_some_and(|alias| index.is_suppressed(&f.file, f.line, alias));
        if suppressed_canonical || suppressed_legacy {
            suppressed_count += 1;
        } else {
            unsuppressed.push(f);
        }
    }

    // 2. Filter baseline multiset (`--baseline`)
    let mut baselined_count = None;
    let mut stale_baseline_entries = Vec::new();
    let mut after_baseline = Vec::new();
    if let Some(base) = baseline {
        let mut remaining_budget: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &base.entries {
            *remaining_budget
                .entry(entry.fingerprint.clone())
                .or_insert(0) += entry.count;
        }
        let mut b_count = 0usize;
        for f in unsuppressed {
            if let Some(rem) = remaining_budget.get_mut(&f.fingerprint) {
                if *rem > 0 {
                    *rem -= 1;
                    b_count += 1;
                    continue;
                }
            }
            after_baseline.push(f);
        }
        let mut stale_total = 0usize;
        for entry in &base.entries {
            if let Some(&rem) = remaining_budget.get(&entry.fingerprint) {
                if rem > 0 {
                    stale_total += rem;
                    let mut st = entry.clone();
                    st.count = rem;
                    stale_baseline_entries.push(st);
                }
            }
        }
        baselined_count = Some(b_count);
        let _ = stale_total;
    } else {
        after_baseline = unsuppressed;
    }

    // 3. Filter affected findings (`--patch-file` / `--git-base`; explicit legacy lines mode)
    let mut outside_changes_count = None;
    let final_findings = if let Some(patch) = patch_text {
        let file_ranges = parse_unified_diff(patch);
        let impact = impact::PatchImpact::new(index, &file_ranges);
        let mut out_cnt = 0usize;
        let mut kept = Vec::new();
        for f in after_baseline {
            let in_patch = file_ranges.iter().any(|(pfile, ranges)| {
                (f.file == *pfile || f.file.ends_with(&format!("/{pfile}")))
                    && ranges
                        .iter()
                        .any(|&(s, e)| f.line >= s && f.line <= e.max(s))
            });
            let evidence_changed = f.evidence.iter().any(|step| {
                file_ranges.get(&step.file).is_some_and(|ranges| {
                    ranges
                        .iter()
                        .any(|&(start, end)| step.line >= start && step.line <= end.max(start))
                })
            });
            if in_patch
                || (!cfg.diff_lines_only && (evidence_changed || impact.contains(&f, index)))
            {
                kept.push(f);
            } else {
                out_cnt += 1;
            }
        }
        outside_changes_count = Some(out_cnt);
        kept
    } else {
        after_baseline
    };

    let mut analyzed = 0usize;
    let mut failed = 0usize;
    let mut module_rows = Vec::new();
    for m in &index.meta.modules {
        let status_str = match m.outcome {
            ParseOutcome::Indexed => {
                analyzed += 1;
                "analyzed"
            }
            ParseOutcome::Partial => {
                failed += 1;
                "partial"
            }
            ParseOutcome::Failed => {
                failed += 1;
                "failed"
            }
        };
        module_rows.push(ModuleStatusRow {
            file: m.file.clone(),
            module: m.module_name.clone(),
            status: status_str.to_string(),
            detail: m.error_detail.clone(),
        });
    }

    let stale_opt = if baseline.is_some() {
        Some(stale_baseline_entries.iter().map(|e| e.count).sum())
    } else {
        None
    };

    CheckRunReport {
        analysis: AnalysisCoverage {
            traversal: "cycle_safe_no_depth_limit".to_string(),
            call_edges: index.calls.len(),
            internal_call_edges: index.calls.iter().filter(|e| index.entity_by_qname.contains_key(&e.callee)).count(),
            external_or_unresolved_call_edges: index.calls.iter().filter(|e| !index.entity_by_qname.contains_key(&e.callee)).count(),
            uncertain_call_edges: index.calls.iter().filter(|e| e.confidence != EdgeConfidence::Proven).count(),
            entry_points: index.entities.iter().filter(|e| e.is_entry_point).count(),
            limitation: "Static graph only: unresolved dynamic calls, external implementations, and undiscovered framework entry points may hide behavior. Patch impact uses the current graph; use a full scan with a baseline to detect consequences of removed edges.".to_string(),
        },
        summary: CheckSummary {
            modules: index.meta.modules.len(),
            analyzed,
            failed,
            findings: final_findings.len(),
            suppressed: suppressed_count,
            baselined: baselined_count,
            stale_baseline: stale_opt,
            outside_changes: outside_changes_count,
        },
        modules: module_rows,
        findings: final_findings,
        stale_baseline: stale_baseline_entries,
    }
}

pub fn build_baseline_file(findings: &[Finding]) -> BaselineFile {
    let mut grouped: BTreeMap<String, BaselineEntry> = BTreeMap::new();
    for f in findings {
        let entry = grouped
            .entry(f.fingerprint.clone())
            .or_insert_with(|| BaselineEntry {
                fingerprint: f.fingerprint.clone(),
                count: 0,
                check: f.check.clone(),
                file: f.file.clone(),
                message: f.message.clone(),
            });
        entry.count += 1;
    }
    BaselineFile {
        version: 1,
        entries: grouped.into_values().collect(),
    }
}

// ---------------------------------------------------------------------------
// Reporters: Text, JSON, SARIF 2.1.0
// ---------------------------------------------------------------------------

pub fn render_text_report(report: &CheckRunReport) -> String {
    render_text_report_verbose(report, 0)
}

pub fn render_text_report_verbose(report: &CheckRunReport, verbosity: u8) -> String {
    if report.findings.is_empty() && report.summary.failed == 0 {
        return "check: no issues found.\n".to_string();
    }
    let mut out = String::new();
    for f in &report.findings {
        out.push_str(&format!(
            "{}:{}:{}: {}: [{}] {}\n",
            f.file,
            f.line,
            f.column,
            f.severity.as_str(),
            f.check,
            if verbosity >= 2 {
                let full = f.full_path();
                f.message
                    .replace(&evidence::compact_path(&full), &full.join(" -> "))
            } else {
                f.message.clone()
            }
        ));
        if verbosity > 0 && !f.evidence.is_empty() {
            let len = f.evidence.len();
            for (i, step) in f.evidence.iter().enumerate() {
                if verbosity < 2 && len > 6 && i >= 3 && i < len - 3 {
                    if i == 3 {
                        out.push_str(&format!(
                            "  ... ({} call sites omitted; use -vv)\n",
                            len - 6
                        ));
                    }
                    continue;
                }
                out.push_str(&format!(
                    "  {}:{}:{}: {} -> {} ({:?})\n",
                    step.file, step.line, step.column, step.caller, step.callee, step.confidence
                ));
            }
        }
    }
    out
}

pub fn render_json_report(report: &CheckRunReport, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(report).unwrap_or_default() + "\n"
    } else {
        serde_json::to_string(report).unwrap_or_default() + "\n"
    }
}

pub fn render_sarif_report(report: &CheckRunReport) -> String {
    let mut rule_ids = BTreeSet::new();
    for f in &report.findings {
        rule_ids.insert(f.check.clone());
    }
    let mut rules = Vec::new();
    for rid in &rule_ids {
        if let Some(desc) = find_check_descriptor(rid) {
            rules.push(json!({
                "id": desc.name,
                "shortDescription": {
                    "text": desc.description,
                },
                "helpUri": format!("docs/checks/{}.md", desc.name),
                "defaultConfiguration": {
                    "level": desc.default_severity.sarif_level(),
                },
                "properties": {
                    "category": desc.category,
                }
            }));
        }
    }

    let results: Vec<Value> = report
        .findings
        .iter()
        .map(|f| {
            let mut result = json!({
                "ruleId": f.check,
                "level": f.severity.sarif_level(),
                "message": {
                    "text": f.message,
                },
                "locations": [
                    {
                        "physicalLocation": {
                            "artifactLocation": {
                                "uri": f.file,
                                "uriBaseId": "%SRCROOT%",
                            },
                            "region": {
                                "startLine": f.line,
                                "startColumn": f.column,
                            }
                        }
                    }
                ],
                "partialFingerprints": {
                    "pyrxFingerprint/v1": f.fingerprint,
                    "pyrexFingerprint/v1": f.fingerprint,
                }
            });
            if !f.evidence.is_empty() {
                result["codeFlows"] = json!([{"threadFlows": [{"locations": f.evidence.iter().map(|step| json!({
                    "location": {
                        "message": {"text": format!("{} -> {}", step.caller, step.callee)},
                        "physicalLocation": {
                            "artifactLocation": {"uri": step.file, "uriBaseId": "%SRCROOT%"},
                            "region": {"startLine": step.line, "startColumn": step.column}
                        }
                    },
                    "properties": {"confidence": step.confidence}
                })).collect::<Vec<_>>()}]}]);
            }
            result
        })
        .collect();

    let sarif = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "pyRx",
                        "version": crate::VERSION,
                        "informationUri": "https://github.com/bearbones/pyRx",
                        "rules": rules,
                    }
                },
                "results": results,
                "properties": {"analysis": report.analysis},
            }
        ]
    });

    serde_json::to_string_pretty(&sarif).unwrap_or_default() + "\n"
}
