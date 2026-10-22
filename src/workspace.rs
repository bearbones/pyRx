use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Represents a resolved Python source file in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceModule {
    /// Logical Python module qualified name, e.g. `pkg.sub.mod` or `pkg.sub` (for `__init__.py`).
    pub module_name: String,
    /// Root-relative forward-slash path, e.g. `src/pkg/sub/mod.py`.
    pub rel_path: String,
    /// Absolute filesystem path used to read the file.
    pub abs_path: PathBuf,
    /// True if this file is an `__init__.py` package initializer.
    pub is_package_init: bool,
    /// True if this file lives under a test directory (`tests/`, `test/`) or is named `test_*.py` / `*_test.py`.
    pub is_test_module: bool,
}

/// Discovered workspace metadata (`pyproject.toml`, `.pyrx.toml`, `src/` roots, entry points).
#[derive(Clone, Debug, Default)]
pub struct WorkspaceConfig {
    pub framework_entry_points: bool,
    pub project_root: PathBuf,
    pub package_roots: Vec<PathBuf>,
    /// Entry points from `[project.scripts]`, `[project.gui-scripts]`, `.pyrx.toml`, `SKILL.md` scripts, and CLI flags.
    /// Format: `module.path:callable` or `module.path`.
    pub entry_points: Vec<String>,
    /// Subset of `entry_points` discovered from `SKILL.md` `scripts/*.py` files (`main_guard` scripts).
    pub skill_entry_points: BTreeSet<String>,
    pub project_name: Option<String>,
    pub requires_python: Option<String>,
    pub source_map: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct SourceSelectionOptions {
    pub no_cache: bool,
    pub project_root: Option<PathBuf>,
    pub sources: Vec<PathBuf>,
    pub source_list: Option<PathBuf>,
    pub source_re: Option<String>,
    pub skip_paths: Vec<String>,
    pub extra_entry_points: Vec<String>,
    /// Optional explicit module name override when indexing a single file (`--module-name`).
    pub module_name_override: Option<String>,
}

impl WorkspaceConfig {
    pub fn discover(start_dir: &Path) -> Self {
        let project_root = find_project_root(start_dir).unwrap_or_else(|| {
            fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf())
        });
        Self::at_root(&project_root)
    }

    /// Explicit project roots must not inherit a containing repository's package roots.
    pub fn at_root(root: &Path) -> Self {
        let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut config = WorkspaceConfig {
            framework_entry_points: true,
            project_root: project_root.clone(),
            package_roots: Vec::new(),
            entry_points: Vec::new(),
            skill_entry_points: BTreeSet::new(),
            project_name: None,
            requires_python: None,
            source_map: BTreeMap::new(),
        };

        // Check for src/ layout vs flat layout
        let src_dir = project_root.join("src");
        if src_dir.is_dir() {
            config.package_roots.push(src_dir);
        }
        config.package_roots.push(project_root.clone());

        // Parse pyproject.toml if present
        let pyproject_path = project_root.join("pyproject.toml");
        if pyproject_path.is_file() {
            if let Ok(content) = fs::read_to_string(&pyproject_path) {
                parse_pyproject_toml(&content, &project_root, &mut config);
            }
        }

        // Parse .pyrx.toml / .pyrex.toml if present for configured entry_points
        for toml_name in [".pyrx.toml", ".pyrex.toml"] {
            let p = project_root.join(toml_name);
            if p.is_file() {
                if let Ok(content) = fs::read_to_string(&p) {
                    parse_pyrx_toml_entry_points(&content, &mut config);
                }
            }
        }

        // Prefer the most specific package root before the containing workspace root.
        config.package_roots.sort_by(|a, b| {
            b.components()
                .count()
                .cmp(&a.components().count())
                .then_with(|| a.cmp(b))
        });
        config.package_roots.dedup();

        // Discover SKILL.md scripts (`**/SKILL.md` -> sibling `scripts/*.py`)
        discover_skill_scripts(&project_root, &mut config);

        config.entry_points.sort();
        config.entry_points.dedup();
        config
    }

    /// Computes a root-relative forward-slash path for deterministic storage.
    pub fn rel_path(&self, path: &Path) -> String {
        let abs = if path.is_absolute() {
            normalize_lexical(path)
        } else {
            normalize_lexical(&self.project_root.join(path))
        };
        let root_norm = normalize_lexical(&self.project_root);
        if let Ok(stripped) = abs.strip_prefix(&root_norm) {
            let s = path_to_slash(stripped);
            if !s.is_empty() {
                return s;
            }
        }
        if let Ok(canon_abs) = fs::canonicalize(path) {
            if let Ok(canon_root) = fs::canonicalize(&self.project_root) {
                if let Ok(stripped) = canon_abs.strip_prefix(&canon_root) {
                    let s = path_to_slash(stripped);
                    if !s.is_empty() {
                        return s;
                    }
                }
            }
        }
        path_to_slash(path)
    }

    /// Infers the Python module qualified name and whether it's an `__init__.py` package.
    pub fn infer_module_name(&self, abs_path: &Path, rel_path: &str) -> (String, bool) {
        if let Some(mapped) = self.source_map.get(rel_path) {
            let is_init = rel_path.ends_with("/__init__.py") || rel_path == "__init__.py";
            return (mapped.clone(), is_init);
        }

        // Try walking up while parent directory contains `__init__.py`
        let file_name = abs_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("module.py");
        let is_package_init = file_name == "__init__.py";

        // First check against configured package_roots (e.g. `src/` before project_root)
        let norm_abs = normalize_lexical(abs_path);
        for pkg_root in &self.package_roots {
            let norm_root = normalize_lexical(pkg_root);
            if let Ok(rel_to_pkg) = norm_abs.strip_prefix(&norm_root) {
                let mut parts: Vec<String> = rel_to_pkg
                    .components()
                    .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
                    .collect();
                if !parts.is_empty() {
                    let last = parts.pop().unwrap();
                    if last == "__init__.py" {
                        if !parts.is_empty() {
                            return (parts.join("."), true);
                        }
                    } else if let Some(stem) = last.strip_suffix(".py") {
                        parts.push(stem.to_string());
                        // If the first component is `src` and we matched `project_root`, strip `src`
                        if parts.len() > 1 && parts[0] == "src" {
                            parts.remove(0);
                        }
                        return (parts.join("."), false);
                    }
                }
            }
        }

        // Fallback: derive from rel_path
        let clean = rel_path.trim_start_matches("./");
        let mut parts: Vec<&str> = clean.split('/').filter(|p| !p.is_empty()).collect();
        if parts.first() == Some(&"src") && parts.len() > 1 {
            parts.remove(0);
        }
        if let Some(last) = parts.pop() {
            if last == "__init__.py" {
                if parts.is_empty() {
                    return ("__init__".to_string(), true);
                }
                return (parts.join("."), true);
            }
            let stem = last.strip_suffix(".py").unwrap_or(last);
            parts.push(stem);
            return (parts.join("."), is_package_init);
        }

        ("unknown".to_string(), is_package_init)
    }
}

/// Resolves a relative import (`from .foo import bar` or `from .. import baz`) into a target module QName.
pub fn resolve_relative_import(
    importer_module: &str,
    is_package_init: bool,
    level: u32,
    module_suffix: Option<&str>,
) -> String {
    if level == 0 {
        return module_suffix.unwrap_or("").to_string();
    }
    let mut parts: Vec<&str> = if importer_module.is_empty() {
        Vec::new()
    } else {
        importer_module.split('.').collect()
    };
    // For a regular module `pkg.sub.mod`, level 1 drops `mod` to reach `pkg.sub`.
    // For a package `__init__.py` whose module_name is already `pkg.sub`, level 1 stays at `pkg.sub`.
    let pops = if is_package_init {
        level.saturating_sub(1) as usize
    } else {
        level as usize
    };
    for _ in 0..pops {
        parts.pop();
    }
    if let Some(suf) = module_suffix {
        if !suf.is_empty() {
            for seg in suf.split('.') {
                parts.push(seg);
            }
        }
    }
    if parts.is_empty() {
        module_suffix.unwrap_or(".").to_string()
    } else {
        parts.join(".")
    }
}

/// Selects and resolves all Python source files according to `SourceSelectionOptions`.
pub fn select_sources(
    opts: &SourceSelectionOptions,
) -> Result<(WorkspaceConfig, Vec<SourceModule>), String> {
    let start_dir = opts
        .project_root
        .clone()
        .or_else(|| {
            opts.sources
                .first()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut ws = if let Some(explicit_root) = &opts.project_root {
        WorkspaceConfig::at_root(explicit_root)
    } else {
        WorkspaceConfig::discover(&start_dir)
    };

    for ep in &opts.extra_entry_points {
        if !ws.entry_points.contains(ep) {
            ws.entry_points.push(ep.clone());
        }
    }
    ws.entry_points.sort();
    ws.entry_points.dedup();

    let mut raw_files: Vec<PathBuf> = Vec::new();
    for s in &opts.sources {
        collect_py_files_from_path(s, &mut raw_files)?;
    }

    if let Some(list_path) = &opts.source_list {
        let content = if list_path.as_os_str() == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("Failed to read --source-list from stdin: {e}"))?;
            buf
        } else {
            fs::read_to_string(list_path).map_err(|e| {
                format!(
                    "Failed to read --source-list '{}': {e}",
                    list_path.display()
                )
            })?
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Support optional `module_name=path.py` syntax for Buck2 source maps
            if let Some((mod_name, file_part)) = trimmed.split_once('=') {
                if file_part.ends_with(".py") && !mod_name.contains('/') {
                    let p = if Path::new(file_part).is_absolute() {
                        PathBuf::from(file_part)
                    } else {
                        ws.project_root.join(file_part)
                    };
                    let rel = ws.rel_path(&p);
                    ws.source_map.insert(rel, mod_name.trim().to_string());
                    collect_py_files_from_path(&p, &mut raw_files)?;
                    continue;
                }
            }
            let p = if Path::new(trimmed).is_absolute() {
                PathBuf::from(trimmed)
            } else {
                ws.project_root.join(trimmed)
            };
            collect_py_files_from_path(&p, &mut raw_files)?;
        }
    }

    // If no explicit sources or source-list were given, walk the workspace project_root
    if opts.sources.is_empty() && opts.source_list.is_none() {
        collect_py_files_from_path(&ws.project_root, &mut raw_files)?;
    }

    let regex_filter = if let Some(re_str) = &opts.source_re {
        Some(Regex::new(re_str).map_err(|e| format!("Invalid --source-re regex '{re_str}': {e}"))?)
    } else {
        None
    };

    let skip_glob = build_skip_globset(&opts.skip_paths)?;

    let mut modules_by_rel: BTreeMap<String, SourceModule> = BTreeMap::new();
    for path in raw_files {
        let abs_path = if path.is_absolute() {
            path
        } else {
            ws.project_root.join(path)
        };
        let rel_path = ws.rel_path(&abs_path);

        if should_skip_path(&rel_path, &opts.skip_paths, &skip_glob) {
            continue;
        }
        if let Some(re) = &regex_filter {
            if !re.is_match(&rel_path) && !re.is_match(&abs_path.to_string_lossy()) {
                continue;
            }
        }

        let (mut module_name, is_package_init) = ws.infer_module_name(&abs_path, &rel_path);
        if let Some(override_name) = &opts.module_name_override {
            if opts.sources.len() == 1 {
                module_name = override_name.clone();
            }
        }
        let is_test_module = is_test_path(&rel_path, &module_name);

        modules_by_rel.insert(
            rel_path.clone(),
            SourceModule {
                module_name,
                rel_path,
                abs_path,
                is_package_init,
                is_test_module,
            },
        );
    }

    let modules: Vec<SourceModule> = modules_by_rel.into_values().collect();
    Ok((ws, modules))
}

fn collect_py_files_from_path(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("Source path does not exist: '{}'", path.display()));
    }
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    for entry in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
    {
        let entry = entry.map_err(|e| format!("Directory walk error: {e}"))?;
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension().and_then(|s| s.to_str()) {
                if ext == "py" {
                    out.push(entry.path().to_path_buf());
                }
            }
        }
    }
    Ok(())
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".pytest_cache"
            | ".pyrx"
            | ".pyrex"
            | "node_modules"
            | "target"
            | "buck-out"
            | "build"
            | "dist"
    ) || name.ends_with(".egg-info")
}

fn build_skip_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.contains('*') || pat.contains('?') || pat.contains('[') {
            let glob =
                Glob::new(pat).map_err(|e| format!("Invalid --skip-paths glob '{pat}': {e}"))?;
            builder.add(glob);
        }
    }
    let set = builder
        .build()
        .map_err(|e| format!("Failed to compile --skip-paths globs: {e}"))?;
    Ok(Some(set))
}

fn should_skip_path(rel_path: &str, patterns: &[String], globset: &Option<GlobSet>) -> bool {
    if let Some(gs) = globset {
        if gs.is_match(rel_path) {
            return true;
        }
    }
    for pat in patterns {
        let clean = pat.trim_matches('/');
        if !clean.is_empty()
            && (rel_path == clean
                || rel_path.starts_with(&format!("{clean}/"))
                || rel_path.contains(&format!("/{clean}/"))
                || rel_path.contains(pat.as_str()))
        {
            return true;
        }
    }
    false
}

pub fn is_test_path(rel_path: &str, module_name: &str) -> bool {
    let parts: Vec<&str> = rel_path.split('/').collect();
    for p in &parts {
        if *p == "tests" || *p == "test" || *p == "testing" {
            return true;
        }
    }
    if let Some(last) = parts.last() {
        if last.starts_with("test_") || last.ends_with("_test.py") || *last == "conftest.py" {
            return true;
        }
    }
    for seg in module_name.split('.') {
        if seg == "tests" || seg == "test" || seg.starts_with("test_") || seg.ends_with("_test") {
            return true;
        }
    }
    false
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    let init = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    let canon = fs::canonicalize(init).unwrap_or_else(|_| init.to_path_buf());
    let mut cur: Option<&Path> = Some(&canon);
    while let Some(dir) = cur {
        if dir.join("pyproject.toml").is_file()
            || dir.join("uv.lock").is_file()
            || dir.join(".pyrx.toml").is_file()
            || dir.join(".pyrex.toml").is_file()
            || dir.join(".pyrx-check.json").is_file()
            || dir.join(".pyrex-check.json").is_file()
            || dir.join(".buckconfig").is_file()
            || dir.join(".git").exists()
        {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    Some(canon)
}

fn parse_pyproject_toml(content: &str, project_root: &Path, config: &mut WorkspaceConfig) {
    let Ok(val) = content.parse::<toml::Value>() else {
        return;
    };
    if let Some(project) = val.get("project").and_then(|v| v.as_table()) {
        if let Some(name) = project.get("name").and_then(|v| v.as_str()) {
            config.project_name = Some(name.to_string());
        }
        if let Some(req_py) = project.get("requires-python").and_then(|v| v.as_str()) {
            config.requires_python = Some(req_py.to_string());
        }
        for section_key in ["scripts", "gui-scripts"] {
            if let Some(scripts) = project.get(section_key).and_then(|v| v.as_table()) {
                for (_script_name, target) in scripts {
                    if let Some(target_str) = target.as_str() {
                        config.entry_points.push(target_str.to_string());
                    }
                }
            }
        }
        if let Some(ep_groups) = project.get("entry-points").and_then(|v| v.as_table()) {
            for (_group_name, group_tbl) in ep_groups {
                if let Some(tbl) = group_tbl.as_table() {
                    for (_ep_name, target) in tbl {
                        if let Some(target_str) = target.as_str() {
                            config.entry_points.push(target_str.to_string());
                        }
                    }
                }
            }
        }
    }
    // Check tool.pyrx / tool.pyrex and tool.uv.workspace
    if let Some(tool) = val.get("tool").and_then(|v| v.as_table()) {
        for tool_key in ["pyrx", "pyrex"] {
            if let Some(pyrx_tbl) = tool.get(tool_key).and_then(|v| v.as_table()) {
                extract_toml_entry_points(pyrx_tbl, &mut config.entry_points);
                if let Some(enabled) = pyrx_tbl
                    .get("framework_entry_points")
                    .and_then(|v| v.as_bool())
                {
                    config.framework_entry_points = enabled;
                }
            }
        }
        if let Some(uv) = tool.get("uv").and_then(|v| v.as_table()) {
            if let Some(ws) = uv.get("workspace").and_then(|v| v.as_table()) {
                if let Some(members) = ws.get("members").and_then(|v| v.as_array()) {
                    let matches = |key: &str, relative: &str| {
                        ws.get(key)
                            .and_then(|v| v.as_array())
                            .is_some_and(|patterns| {
                                patterns.iter().filter_map(|v| v.as_str()).any(|pattern| {
                                    globset::GlobBuilder::new(pattern.trim_end_matches('/'))
                                        .literal_separator(true)
                                        .build()
                                        .is_ok_and(|glob| glob.compile_matcher().is_match(relative))
                                })
                            })
                    };
                    if !members.is_empty() {
                        for entry in WalkDir::new(project_root)
                            .into_iter()
                            .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
                            .filter_map(Result::ok)
                        {
                            if entry.file_name() != "pyproject.toml" || !entry.file_type().is_file()
                            {
                                continue;
                            }
                            let member_dir = entry.path().parent().unwrap();
                            if member_dir == project_root {
                                continue;
                            }
                            let relative =
                                path_to_slash(member_dir.strip_prefix(project_root).unwrap());
                            if matches("members", &relative) && !matches("exclude", &relative) {
                                let member_src = member_dir.join("src");
                                if member_src.is_dir() {
                                    config.package_roots.push(member_src);
                                }
                                config.package_roots.push(member_dir.to_path_buf());
                                if let Ok(content) = fs::read_to_string(entry.path()) {
                                    if let Ok(member_val) = content.parse::<toml::Value>() {
                                        collect_project_entry_points(
                                            &member_val,
                                            &mut config.entry_points,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn collect_project_entry_points(value: &toml::Value, out: &mut Vec<String>) {
    let Some(project) = value.get("project") else {
        return;
    };
    for section in ["scripts", "gui-scripts"] {
        if let Some(table) = project.get(section).and_then(|v| v.as_table()) {
            out.extend(table.values().filter_map(|v| v.as_str()).map(str::to_owned));
        }
    }
    if let Some(groups) = project.get("entry-points").and_then(|v| v.as_table()) {
        for table in groups.values().filter_map(|v| v.as_table()) {
            out.extend(table.values().filter_map(|v| v.as_str()).map(str::to_owned));
        }
    }
}

fn parse_pyrx_toml_entry_points(content: &str, config: &mut WorkspaceConfig) {
    let Ok(val) = content.parse::<toml::Value>() else {
        return;
    };
    if let Some(tbl) = val.as_table() {
        extract_toml_entry_points(tbl, &mut config.entry_points);
        if let Some(enabled) = tbl.get("framework_entry_points").and_then(|v| v.as_bool()) {
            config.framework_entry_points = enabled;
        }
    }
}

fn extract_toml_entry_points(tbl: &toml::map::Map<String, toml::Value>, out: &mut Vec<String>) {
    if let Some(ep_val) = tbl.get("entry_points") {
        if let Some(arr) = ep_val.as_array() {
            for item in arr.iter().filter_map(|v| v.as_str()) {
                out.push(item.to_string());
            }
        } else if let Some(ep_tbl) = ep_val.as_table() {
            for (_k, v) in ep_tbl {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                } else if let Some(arr) = v.as_array() {
                    for item in arr.iter().filter_map(|x| x.as_str()) {
                        out.push(item.to_string());
                    }
                }
            }
        }
    }
}

fn discover_skill_scripts(project_root: &Path, config: &mut WorkspaceConfig) {
    let mut skill_dirs = BTreeSet::new();
    for entry in WalkDir::new(project_root)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
        .flatten()
    {
        if entry.file_type().is_file() && entry.file_name() == "SKILL.md" {
            if let Some(parent) = entry.path().parent() {
                skill_dirs.insert(parent.to_path_buf());
            }
        }
    }
    for skill_dir in skill_dirs {
        let scripts_dir = skill_dir.join("scripts");
        if scripts_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&scripts_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("py") {
                        let rel = config.rel_path(&p);
                        let (mod_name, _) = config.infer_module_name(&p, &rel);
                        let mod_ep = format!("{mod_name}.<module>");
                        let main_ep = format!("{mod_name}.main");
                        config.skill_entry_points.insert(mod_ep.clone());
                        config.skill_entry_points.insert(main_ep.clone());
                        config.entry_points.push(mod_ep);
                        config.entry_points.push(main_ep);
                    }
                }
            }
        }
    }
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
