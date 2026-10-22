use crate::model::*;
use crate::workspace::{SourceModule, WorkspaceConfig, resolve_relative_import};
use ruff_python_ast::{
    CmpOp, ExceptHandler, Expr, ModModule, Operator, Stmt, StmtClassDef, StmtFunctionDef, UnaryOp,
};
use ruff_python_parser::parse_module;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextRange, TextSize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use xxhash_rust::xxh3::xxh3_64;

/// Indexes a single Python source module into a `ModuleShard`.
pub fn index_module(src: &SourceModule, ws: &WorkspaceConfig) -> ModuleShard {
    let bytes = match fs::read(&src.abs_path) {
        Ok(b) => b,
        Err(e) => {
            return ModuleShard {
                module_name: src.module_name.clone(),
                file: src.rel_path.clone(),
                fingerprint: 0,
                outcome: ParseOutcome::Failed,
                error_detail: Some(format!("Failed to read file: {e}")),
                entities: Vec::new(),
                raw_calls: Vec::new(),
                raw_contexts: Vec::new(),
                imports: Vec::new(),
                schemas: Vec::new(),
                env_accesses: Vec::new(),
                inline_suppressions: Vec::new(),
                import_bindings: BTreeMap::new(),
                dunder_all: None,
                lock_acquisitions: BTreeMap::new(),
                await_under_threading_lock: Vec::new(),
                channels: Vec::new(),
                function_returns: BTreeMap::new(),
            };
        }
    };

    let fingerprint = xxh3_64(&bytes);
    let source_text = String::from_utf8_lossy(&bytes).into_owned();
    index_module_str(
        &src.module_name,
        &src.rel_path,
        src.is_package_init,
        src.is_test_module,
        &source_text,
        fingerprint,
        ws,
    )
}

/// Indexes Python source text directly (used by both filesystem indexing and unit tests).
pub fn index_module_str(
    module_name: &str,
    rel_path: &str,
    is_package_init: bool,
    is_test_module: bool,
    source_text: &str,
    fingerprint: u64,
    ws: &WorkspaceConfig,
) -> ModuleShard {
    let line_index = LineIndex::from_source_text(source_text);
    let inline_suppressions = extract_inline_suppressions(source_text);
    let (sanitized_cow, lazy_import_lines) = preprocess_pep810_lazy_imports(source_text);

    let parsed = parse_module(&sanitized_cow);
    let (ast_mod, outcome, error_detail) = match parsed {
        Ok(p) => (p.into_syntax(), ParseOutcome::Indexed, None),
        Err(err) => {
            return ModuleShard {
                module_name: module_name.to_string(),
                file: rel_path.to_string(),
                fingerprint,
                outcome: ParseOutcome::Failed,
                error_detail: Some(format!("{err}")),
                entities: Vec::new(),
                raw_calls: Vec::new(),
                raw_contexts: Vec::new(),
                imports: Vec::new(),
                schemas: Vec::new(),
                env_accesses: Vec::new(),
                inline_suppressions,
                import_bindings: BTreeMap::new(),
                dunder_all: None,
                lock_acquisitions: BTreeMap::new(),
                await_under_threading_lock: Vec::new(),
                channels: Vec::new(),
                function_returns: BTreeMap::new(),
            };
        }
    };

    let mut indexer = ModuleAstIndexer::new(
        module_name,
        rel_path,
        is_package_init,
        is_test_module,
        source_text,
        &line_index,
        ws,
        lazy_import_lines,
    );
    indexer.visit_module(&ast_mod);

    ModuleShard {
        module_name: module_name.to_string(),
        file: rel_path.to_string(),
        fingerprint,
        outcome,
        error_detail,
        entities: indexer.entities,
        raw_calls: indexer.calls,
        raw_contexts: indexer.contexts,
        imports: indexer.imports,
        schemas: indexer.schemas,
        env_accesses: indexer.env_accesses,
        inline_suppressions,
        import_bindings: indexer.import_bindings,
        dunder_all: indexer.dunder_all,
        lock_acquisitions: indexer.lock_acquisitions,
        await_under_threading_lock: indexer.await_under_threading_lock,
        channels: Vec::new(),
        function_returns: BTreeMap::new(),
    }
}

/// Detects Python 3.15 (PEP 810) `lazy import ...` and `lazy from ... import ...` statements,
/// removing the 5-byte `lazy ` prefix while padding 5 spaces at the end of the line so
/// indentation, line numbers, and subsequent line byte offsets remain identical.
fn preprocess_pep810_lazy_imports(source: &str) -> (std::borrow::Cow<'_, str>, BTreeSet<u32>) {
    let mut lazy_lines = BTreeSet::new();
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("lazy import ") || trimmed.starts_with("lazy from ") {
            lazy_lines.insert((idx + 1) as u32);
        }
    }
    if lazy_lines.is_empty() {
        return (std::borrow::Cow::Borrowed(source), lazy_lines);
    }
    let mut bytes = source.as_bytes().to_vec();
    let mut cur_line = 1u32;
    let mut line_start = 0usize;
    for i in 0..bytes.len() {
        if i == line_start && lazy_lines.contains(&cur_line) {
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let mut line_end = j;
            while line_end < bytes.len() && bytes[line_end] != b'\n' && bytes[line_end] != b'\r' {
                line_end += 1;
            }
            if j + 5 <= line_end && &bytes[j..j + 5] == b"lazy " {
                bytes.copy_within(j + 5..line_end, j);
                bytes[line_end - 5..line_end].copy_from_slice(b"     ");
            }
        }
        if bytes[i] == b'\n' {
            cur_line += 1;
            line_start = i + 1;
        }
    }
    let replaced = String::from_utf8(bytes).unwrap_or_else(|_| source.to_string());
    (std::borrow::Cow::Owned(replaced), lazy_lines)
}

fn expand_suppression_check_aliases(raw: &str) -> Vec<String> {
    match raw {
        "unused-parameter" | "unused-parameter-cross-tu" => vec![
            "unused-parameter".to_string(),
            "unused-parameter-cross-tu".to_string(),
        ],
        "uncentralized-env-access" | "env-sprawl" => vec![
            "uncentralized-env-access".to_string(),
            "env-sprawl".to_string(),
        ],
        other => vec![other.to_string()],
    }
}

fn extract_inline_suppressions(source: &str) -> Vec<InlineSuppression> {
    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        for marker in ["# pyrx: ignore", "# pyrex: ignore"] {
            if let Some(pos) = line.find(marker) {
                let rest = line[pos + marker.len()..].trim();
                if let Some(bracketed) = rest.strip_prefix('[') {
                    if let Some(end) = bracketed.find(']') {
                        let inside = &bracketed[..end];
                        let checks: Vec<String> = inside
                            .split(',')
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .flat_map(expand_suppression_check_aliases)
                            .collect();
                        out.push(InlineSuppression {
                            line: line_no,
                            checks,
                        });
                        break;
                    }
                }
                out.push(InlineSuppression {
                    line: line_no,
                    checks: Vec::new(),
                });
                break;
            }
        }
    }
    out
}

#[derive(Clone)]
struct ScopeFrame {
    qualified_name: String,
    enclosing_class: Option<String>,
    execution_context: ExecutionContext,
    is_function_like: bool,
    var_types: BTreeMap<String, String>,
    var_locks: BTreeMap<String, (ResourceKind, String)>,
    var_call_origins: BTreeMap<String, String>,
    var_resource_allocs: BTreeMap<String, (String, ResourceKind, u32, u32)>,
    var_dict_keys: BTreeMap<String, BTreeSet<String>>,
    local_functions: BTreeMap<String, String>,
    guarded_dict_keys: BTreeSet<(String, String)>,
    param_names: BTreeSet<String>,
    used_params: BTreeSet<String>,
    raises: Vec<RaiseSite>,
    broad_catches: Vec<CatchSiteInfo>,
    returned_resources: Vec<ResourceAllocSite>,
    dict_returns: Vec<DictShapeSite>,
    dict_accesses: Vec<DictAccessSite>,
    returns_optional_or_status: bool,
    lambda_counter: u32,
    #[allow(dead_code)]
    comp_counter: u32,
}

struct ModuleAstIndexer<'a> {
    module_name: &'a str,
    rel_path: &'a str,
    is_package_init: bool,
    is_test_module: bool,
    source: &'a str,
    line_index: &'a LineIndex,
    ws: &'a WorkspaceConfig,
    lazy_import_lines: BTreeSet<u32>,

    entities: Vec<EntityNode>,
    calls: Vec<CallEdge>,
    contexts: Vec<CallSiteContext>,
    imports: Vec<ImportEdge>,
    schemas: Vec<ClassSchema>,
    env_accesses: Vec<EnvAccess>,
    import_bindings: BTreeMap<String, (String, Option<String>)>,
    local_classes: BTreeSet<String>,
    local_defs: BTreeSet<String>,
    class_attr_locks: BTreeMap<(String, String), (ResourceKind, String)>,
    module_locks: BTreeMap<String, (ResourceKind, String)>,
    dunder_all: Option<Vec<String>>,
    has_main_guard: bool,

    scope_stack: Vec<ScopeFrame>,
    try_stack: Vec<TryScope>,
    with_stack: Vec<WithScope>,
    guard_stack: Vec<GuardScope>,
    await_depth: u32,
    in_mutation_target: u32,
    cast_context: Option<String>,
    lock_acquisitions: BTreeMap<String, Vec<(String, u32, bool)>>,
    await_under_threading_lock: Vec<(String, String, u32, u32)>,
}

impl<'a> ModuleAstIndexer<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        module_name: &'a str,
        rel_path: &'a str,
        is_package_init: bool,
        is_test_module: bool,
        source: &'a str,
        line_index: &'a LineIndex,
        ws: &'a WorkspaceConfig,
        lazy_import_lines: BTreeSet<u32>,
    ) -> Self {
        Self {
            module_name,
            rel_path,
            is_package_init,
            is_test_module,
            source,
            line_index,
            ws,
            lazy_import_lines,
            entities: Vec::new(),
            calls: Vec::new(),
            contexts: Vec::new(),
            imports: Vec::new(),
            schemas: Vec::new(),
            env_accesses: Vec::new(),
            import_bindings: BTreeMap::new(),
            local_classes: BTreeSet::new(),
            local_defs: BTreeSet::new(),
            class_attr_locks: BTreeMap::new(),
            module_locks: BTreeMap::new(),
            dunder_all: None,
            has_main_guard: false,
            scope_stack: Vec::new(),
            try_stack: Vec::new(),
            with_stack: Vec::new(),
            guard_stack: Vec::new(),
            await_depth: 0,
            in_mutation_target: 0,
            cast_context: None,
            lock_acquisitions: BTreeMap::new(),
            await_under_threading_lock: Vec::new(),
        }
    }

    fn line_col(&self, offset: TextSize) -> (u32, u32) {
        let lc = self.line_index.line_column(offset, self.source);
        (lc.line.get() as u32, lc.column.get() as u32)
    }

    fn span_info(&self, range: TextRange) -> (u32, u32, u32, u32, u32, u32) {
        let (sl, sc) = self.line_col(range.start());
        let (el, ec) = self.line_col(range.end());
        (sl, sc, el, ec, range.start().to_u32(), range.end().to_u32())
    }

    fn slice_range(&self, range: TextRange) -> &str {
        let start = range.start().to_usize().min(self.source.len());
        let end = range.end().to_usize().min(self.source.len());
        &self.source[start..end]
    }

    fn current_scope(&self) -> &ScopeFrame {
        self.scope_stack.last().expect("scope stack non-empty")
    }

    fn current_scope_mut(&mut self) -> &mut ScopeFrame {
        self.scope_stack.last_mut().expect("scope stack non-empty")
    }

    fn in_type_checking(&self) -> bool {
        self.guard_stack
            .iter()
            .any(|g| g.kind == GuardKind::TypeChecking && g.in_true_branch)
    }

    fn visit_module(&mut self, module: &ModModule) {
        // Pre-scan top-level definitions and __all__ so forward references within the module resolve cleanly
        for stmt in &module.body {
            match stmt {
                Stmt::FunctionDef(f) => {
                    self.local_defs.insert(f.name.id.to_string());
                }
                Stmt::ClassDef(c) => {
                    self.local_classes.insert(c.name.id.to_string());
                    self.local_defs.insert(c.name.id.to_string());
                }
                Stmt::Assign(assign) => {
                    for target in &assign.targets {
                        if let Expr::Name(n) = target {
                            if n.id.as_str() == "__all__" {
                                if let Some(items) = extract_string_list(&assign.value) {
                                    self.dunder_all = Some(items);
                                }
                            }
                        }
                    }
                }
                Stmt::AnnAssign(ann) => {
                    if let Expr::Name(n) = &*ann.target {
                        if n.id.as_str() == "__all__" {
                            if let Some(val) = &ann.value {
                                if let Some(items) = extract_string_list(val) {
                                    self.dunder_all = Some(items);
                                }
                            }
                        }
                    }
                }
                Stmt::AugAssign(aug) => {
                    if let Expr::Name(n) = &*aug.target {
                        if n.id.as_str() == "__all__" {
                            if let Some(items) = extract_string_list(&aug.value) {
                                self.dunder_all.get_or_insert_with(Vec::new).extend(items);
                            }
                        }
                    }
                }
                Stmt::Expr(expr_stmt) => {
                    if let Expr::Call(call) = &*expr_stmt.value {
                        if let Expr::Attribute(attr) = &*call.func {
                            if let Expr::Name(n) = &*attr.value {
                                if n.id.as_str() == "__all__" {
                                    if attr.attr.id.as_str() == "extend" {
                                        if let Some(first_arg) = call.arguments.args.first() {
                                            if let Some(items) = extract_string_list(first_arg) {
                                                self.dunder_all
                                                    .get_or_insert_with(Vec::new)
                                                    .extend(items);
                                            }
                                        }
                                    } else if attr.attr.id.as_str() == "append" {
                                        if let Some(first_arg) = call.arguments.args.first() {
                                            if let Some(s) = extract_string_literal(first_arg) {
                                                self.dunder_all
                                                    .get_or_insert_with(Vec::new)
                                                    .push(s);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Stmt::If(if_stmt) => {
                    let cond_str = self.slice_range(if_stmt.test.range()).trim().to_string();
                    if classify_guard(&if_stmt.test, &cond_str) == GuardKind::MainGuard {
                        self.has_main_guard = true;
                    }
                }
                _ => {}
            }
        }

        let mod_qname = format!("{}.<module>", self.module_name);
        let mod_usr = format!("py:{}#<module>", self.module_name);
        self.scope_stack.push(ScopeFrame {
            qualified_name: mod_qname.clone(),
            enclosing_class: None,
            execution_context: ExecutionContext::ModuleInit,
            is_function_like: false,
            var_types: BTreeMap::new(),
            var_locks: BTreeMap::new(),
            var_call_origins: BTreeMap::new(),
            var_resource_allocs: BTreeMap::new(),
            var_dict_keys: BTreeMap::new(),
            local_functions: BTreeMap::new(),
            guarded_dict_keys: BTreeSet::new(),
            param_names: BTreeSet::new(),
            used_params: BTreeSet::new(),
            raises: Vec::new(),
            broad_catches: Vec::new(),
            returned_resources: Vec::new(),
            dict_returns: Vec::new(),
            dict_accesses: Vec::new(),
            returns_optional_or_status: false,
            lambda_counter: 0,
            comp_counter: 0,
        });

        for stmt in &module.body {
            self.visit_stmt(stmt);
        }

        let mod_frame = self.scope_stack.pop().unwrap();
        let (sl, sc, el, ec, bs, be) = self.span_info(module.range);
        let is_configured_ep = self.is_configured_entry_point(&mod_qname, "<module>");
        let is_skill_ep = self.is_skill_script_entry_point(&mod_qname);
        let is_ep = self.has_main_guard || is_configured_ep || is_skill_ep || self.is_test_module;
        let ep_kind = if self.has_main_guard || is_skill_ep {
            Some("main_guard".to_string())
        } else if is_configured_ep {
            Some("pyproject_script".to_string())
        } else if self.is_test_module {
            Some("pytest".to_string())
        } else {
            None
        };

        self.entities.push(EntityNode {
            usr: mod_usr,
            qualified_name: mod_qname,
            name: "<module>".to_string(),
            module: self.module_name.to_string(),
            enclosing_class: None,
            file: self.rel_path.to_string(),
            line: sl.max(1),
            column: sc.max(1),
            end_line: el.max(1),
            end_column: ec.max(1),
            byte_start: bs,
            byte_end: be,
            kind: EntityKind::Module,
            execution_context: ExecutionContext::ModuleInit,
            decorators: Vec::new(),
            parameters: Vec::new(),
            return_annotation: None,
            bases: Vec::new(),
            mro: Vec::new(),
            is_protocol: false,
            is_abc: false,
            is_abstractmethod: false,
            is_exported: true,
            is_entry_point: is_ep,
            entry_point_kind: ep_kind,
            skeleton_hash: 0,
            skeleton_features: Vec::new(),
            normalized_skeleton: String::new(),
            statement_count: module.body.len() as u32,
            raises: mod_frame.raises,
            broad_catches: mod_frame.broad_catches,
            returned_resources: mod_frame.returned_resources,
            dict_returns: mod_frame.dict_returns,
            dict_accesses: mod_frame.dict_accesses,
            returns_optional_or_status: false,
        });

        // Sort entities deterministically by (line, byte_start, qualified_name)
        self.entities.sort_by(|a, b| {
            a.line
                .cmp(&b.line)
                .then_with(|| a.byte_start.cmp(&b.byte_start))
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });
    }

    fn is_configured_entry_point(&self, qname: &str, short_name: &str) -> bool {
        let colon_form = format!("{}:{short_name}", self.module_name);
        for ep in &self.ws.entry_points {
            if ep == qname || ep == &colon_form || ep == self.module_name {
                return true;
            }
        }
        false
    }

    fn is_skill_script_entry_point(&self, qname: &str) -> bool {
        self.ws.skill_entry_points.contains(qname)
            || self.ws.skill_entry_points.contains(self.module_name)
    }

    fn is_exported_name(&self, name: &str, enclosing_class: &Option<String>) -> bool {
        if enclosing_class.is_some() {
            return false;
        }
        if let Some(dunder) = &self.dunder_all {
            return dunder.iter().any(|x| x == name);
        }
        if self.is_package_init && !name.starts_with('_') {
            return true;
        }
        false
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Import(imp) => {
                let (line, col, _, _, bs, be) = self.span_info(imp.range);
                let is_tc = self.in_type_checking();
                let is_lazy =
                    self.current_scope().is_function_like || self.lazy_import_lines.contains(&line);
                let enclosing = self.current_scope().qualified_name.clone();
                for alias in &imp.names {
                    let target = alias.name.id.to_string();
                    let local_name = alias
                        .asname
                        .as_ref()
                        .map(|a| a.id.to_string())
                        .unwrap_or_else(|| target.split('.').next().unwrap_or(&target).to_string());
                    if alias.asname.is_some() {
                        self.import_bindings
                            .insert(local_name.clone(), (target.clone(), None));
                    } else {
                        let top = target.split('.').next().unwrap_or(&target).to_string();
                        self.import_bindings.insert(top, (target.clone(), None));
                    }
                    self.imports.push(ImportEdge {
                        importer_module: self.module_name.to_string(),
                        enclosing_entity: enclosing.clone(),
                        target_module: target,
                        imported_symbol: None,
                        alias: alias.asname.as_ref().map(|a| a.id.to_string()),
                        file: self.rel_path.to_string(),
                        line,
                        column: col,
                        byte_start: bs,
                        byte_end: be,
                        is_type_checking: is_tc,
                        is_lazy,
                        is_relative: false,
                        level: 0,
                    });
                }
            }
            Stmt::ImportFrom(imp) => {
                let (line, col, _, _, bs, be) = self.span_info(imp.range);
                let is_tc = self.in_type_checking();
                let is_lazy =
                    self.current_scope().is_function_like || self.lazy_import_lines.contains(&line);
                let enclosing = self.current_scope().qualified_name.clone();
                let mod_suffix = imp.module.as_ref().map(|m| m.id.as_str());
                let resolved_mod = if imp.level > 0 {
                    resolve_relative_import(
                        self.module_name,
                        self.is_package_init,
                        imp.level,
                        mod_suffix,
                    )
                } else {
                    mod_suffix.unwrap_or("").to_string()
                };

                for alias in &imp.names {
                    let sym = alias.name.id.to_string();
                    let local_name = alias
                        .asname
                        .as_ref()
                        .map(|a| a.id.to_string())
                        .unwrap_or_else(|| sym.clone());
                    self.import_bindings
                        .insert(local_name, (resolved_mod.clone(), Some(sym.clone())));
                    self.imports.push(ImportEdge {
                        importer_module: self.module_name.to_string(),
                        enclosing_entity: enclosing.clone(),
                        target_module: resolved_mod.clone(),
                        imported_symbol: Some(sym),
                        alias: alias.asname.as_ref().map(|a| a.id.to_string()),
                        file: self.rel_path.to_string(),
                        line,
                        column: col,
                        byte_start: bs,
                        byte_end: be,
                        is_type_checking: is_tc,
                        is_lazy,
                        is_relative: imp.level > 0,
                        level: imp.level,
                    });
                }
            }
            Stmt::ClassDef(cls) => self.visit_class_def(cls),
            Stmt::FunctionDef(func) => self.visit_function_def(func),
            Stmt::If(if_stmt) => {
                let (line, _) = self.line_col(if_stmt.range.start());
                let cond_str = self.slice_range(if_stmt.test.range()).trim().to_string();
                let guard_kind = classify_guard(&if_stmt.test, &cond_str);
                if guard_kind == GuardKind::MainGuard {
                    self.has_main_guard = true;
                }
                self.visit_expr(&if_stmt.test);

                self.guard_stack.push(GuardScope {
                    kind: guard_kind,
                    expr: cond_str.clone(),
                    in_true_branch: true,
                    line,
                });
                for s in &if_stmt.body {
                    self.visit_stmt(s);
                }
                self.guard_stack.pop();

                let mut prev_guards = vec![(guard_kind, cond_str.clone(), line)];
                for clause in &if_stmt.elif_else_clauses {
                    for (gk, ge, gl) in &prev_guards {
                        self.guard_stack.push(GuardScope {
                            kind: *gk,
                            expr: ge.clone(),
                            in_true_branch: false,
                            line: *gl,
                        });
                    }
                    if let Some(test) = &clause.test {
                        let (cl_line, _) = self.line_col(test.range().start());
                        let cl_str = self.slice_range(test.range()).trim().to_string();
                        let cl_kind = classify_guard(test, &cl_str);
                        self.visit_expr(test);
                        self.guard_stack.push(GuardScope {
                            kind: cl_kind,
                            expr: cl_str.clone(),
                            in_true_branch: true,
                            line: cl_line,
                        });
                        for s in &clause.body {
                            self.visit_stmt(s);
                        }
                        self.guard_stack.pop();
                        prev_guards.push((cl_kind, cl_str, cl_line));
                    } else {
                        for s in &clause.body {
                            self.visit_stmt(s);
                        }
                    }
                    for _ in 0..prev_guards.len().saturating_sub(if clause.test.is_some() {
                        1
                    } else {
                        0
                    }) {
                        self.guard_stack.pop();
                    }
                }

                // If `if "k" not in d: return/raise/continue/break`, then after the `if` block
                // `"k"` is guaranteed to be in `d` for the rest of the current scope.
                if if_stmt.elif_else_clauses.is_empty() && suite_always_exits(&if_stmt.body) {
                    for pair in extract_negated_dict_key_guards(&cond_str) {
                        self.current_scope_mut().guarded_dict_keys.insert(pair);
                    }
                }
            }
            Stmt::With(with_stmt) => {
                let (line, _) = self.line_col(with_stmt.range.start());
                let mut pushed = 0usize;
                for item in &with_stmt.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(opt_var) = &item.optional_vars {
                        self.visit_expr(opt_var);
                    }
                    let ctx_str = self
                        .slice_range(item.context_expr.range())
                        .trim()
                        .to_string();
                    let var_name = item
                        .optional_vars
                        .as_ref()
                        .map(|v| self.slice_range(v.range()).trim().to_string());

                    // Check if the context expression is a known variable holding an allocated resource;
                    // if so, mark it as owned by `with` so `unowned-resource-return` won't flag it.
                    if let Expr::Name(n) = &item.context_expr {
                        self.current_scope_mut()
                            .var_resource_allocs
                            .remove(n.id.as_str());
                    }

                    let (res_kind, lock_id) = self.classify_with_resource(
                        &item.context_expr,
                        &ctx_str,
                        with_stmt.is_async,
                    );
                    if let Some(lid) = &lock_id {
                        let scope_qname = self.current_scope().qualified_name.clone();
                        self.lock_acquisitions
                            .entry(scope_qname)
                            .or_default()
                            .push((lid.clone(), line, with_stmt.is_async));
                    }
                    self.with_stack.push(WithScope {
                        context_expr: ctx_str,
                        var_name,
                        resource_kind: res_kind,
                        lock_identity: lock_id,
                        is_async: with_stmt.is_async,
                        line,
                    });
                    pushed += 1;
                }
                let contexts_before = self.contexts.len();
                for s in &with_stmt.body {
                    self.visit_stmt(s);
                }
                if self.contexts.len() == contexts_before && !self.with_stack.is_empty() {
                    let (_, col) = self.line_col(with_stmt.range.start());
                    let context_id = self.contexts.len() as u32;
                    let caller = self.current_scope().qualified_name.clone();
                    let exec_ctx = self.current_scope().execution_context;
                    self.contexts.push(CallSiteContext {
                        context_id,
                        caller,
                        callee: "<with>".to_string(),
                        file: self.rel_path.to_string(),
                        line,
                        column: col,
                        execution_context: exec_ctx,
                        is_awaited: with_stmt.is_async,
                        try_scopes: self.try_stack.clone(),
                        with_scopes: self.with_stack.clone(),
                        guards: self.guard_stack.clone(),
                    });
                }
                for _ in 0..pushed {
                    self.with_stack.pop();
                }
            }
            Stmt::Try(try_stmt) => {
                let (try_line, _) = self.line_col(try_stmt.range.start());
                let mut handler_scopes = Vec::new();
                for h in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(eh) = h;
                    let (h_line, h_col) = self.line_col(eh.range.start());
                    let caught_types =
                        extract_caught_types(&eh.type_, |e| self.resolve_expr_callee(e));
                    let is_bare = eh.type_.is_none();
                    let is_broad = is_bare
                        || caught_types.iter().any(|t| {
                            t == "Exception"
                                || t == "BaseException"
                                || t.ends_with(".Exception")
                                || t.ends_with(".BaseException")
                        });
                    let rethrows = suite_rethrows(&eh.body);
                    let handles_cancelled = suite_handles_cancelled_error(&eh.body, &caught_types);
                    let returns_assumed_ok = suite_returns_assumed_ok(&eh.body);
                    let swallows = !rethrows && is_broad && suite_swallows_exception(&eh.body);

                    handler_scopes.push(HandlerScope {
                        caught_types: caught_types.clone(),
                        is_bare_except: is_bare,
                        rethrows,
                        swallows,
                        line: h_line,
                    });

                    if is_broad {
                        self.current_scope_mut().broad_catches.push(CatchSiteInfo {
                            caught_types,
                            is_bare_except: is_bare,
                            is_broad,
                            rethrows,
                            swallows,
                            handles_cancelled_error: handles_cancelled,
                            returns_assumed_ok_value: returns_assumed_ok,
                            line: h_line,
                            column: h_col,
                        });
                    }
                }

                self.try_stack.push(TryScope {
                    handlers: handler_scopes,
                    has_finally: !try_stmt.finalbody.is_empty(),
                    line: try_line,
                });
                for s in &try_stmt.body {
                    self.visit_stmt(s);
                }
                self.try_stack.pop();

                for h in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(eh) = h;
                    if let Some(ty) = &eh.type_ {
                        self.visit_expr(ty);
                    }
                    for s in &eh.body {
                        self.visit_stmt(s);
                    }
                }
                for s in &try_stmt.orelse {
                    self.visit_stmt(s);
                }
                for s in &try_stmt.finalbody {
                    self.visit_stmt(s);
                }
            }
            Stmt::Raise(raise_stmt) => {
                let (line, col) = self.line_col(raise_stmt.range.start());
                if let Some(exc) = &raise_stmt.exc {
                    self.visit_expr(exc);
                    let exc_type = self.extract_exception_type(exc);
                    let msg_expr = Some(self.slice_range(exc.range()).trim().to_string());
                    let is_caught = self.is_exception_caught_locally(&exc_type);
                    self.current_scope_mut().raises.push(RaiseSite {
                        exception_type: exc_type,
                        message_expr: msg_expr,
                        is_explicit: true,
                        is_caught_locally: is_caught,
                        line,
                        column: col,
                    });
                }
                if let Some(cause) = &raise_stmt.cause {
                    self.visit_expr(cause);
                }
            }
            Stmt::Return(ret_stmt) => {
                if let Some(val) = &ret_stmt.value {
                    self.visit_expr(val);
                    if is_none_or_false_expr(val) {
                        self.current_scope_mut().returns_optional_or_status = true;
                    }
                    self.record_return_facts(val);
                } else {
                    self.current_scope_mut().returns_optional_or_status = true;
                }
            }
            Stmt::Assign(assign) => {
                self.visit_expr(&assign.value);
                for target in &assign.targets {
                    self.visit_assign_target(target, Some(&assign.value));
                }
            }
            Stmt::AnnAssign(ann) => {
                self.visit_expr(&ann.annotation);
                if let Some(val) = &ann.value {
                    self.visit_expr(val);
                }
                if let Expr::Name(n) = &*ann.target {
                    let ann_type = self.resolve_expr_callee(&ann.annotation);
                    self.current_scope_mut()
                        .var_types
                        .insert(n.id.to_string(), ann_type);
                }
                self.visit_assign_target(&ann.target, ann.value.as_deref());
            }
            Stmt::AugAssign(aug) => {
                self.in_mutation_target += 1;
                self.visit_expr(&aug.target);
                self.in_mutation_target -= 1;
                self.visit_expr(&aug.value);
            }
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.target);
                self.visit_expr(&for_stmt.iter);
                for s in &for_stmt.body {
                    self.visit_stmt(s);
                }
                for s in &for_stmt.orelse {
                    self.visit_stmt(s);
                }
            }
            Stmt::While(while_stmt) => {
                let (line, _) = self.line_col(while_stmt.range.start());
                let cond_str = self.slice_range(while_stmt.test.range()).trim().to_string();
                let guard_kind = classify_guard(&while_stmt.test, &cond_str);
                self.visit_expr(&while_stmt.test);
                self.guard_stack.push(GuardScope {
                    kind: guard_kind,
                    expr: cond_str,
                    in_true_branch: true,
                    line,
                });
                for s in &while_stmt.body {
                    self.visit_stmt(s);
                }
                self.guard_stack.pop();
                for s in &while_stmt.orelse {
                    self.visit_stmt(s);
                }
            }
            Stmt::Match(match_stmt) => {
                self.visit_expr(&match_stmt.subject);
                for case in &match_stmt.cases {
                    if let Some(g) = &case.guard {
                        self.visit_expr(g);
                    }
                    for s in &case.body {
                        self.visit_stmt(s);
                    }
                }
            }
            Stmt::Assert(assert_stmt) => {
                let (line, _) = self.line_col(assert_stmt.range.start());
                let expr_str = self
                    .slice_range(assert_stmt.test.range())
                    .trim()
                    .to_string();
                self.visit_expr(&assert_stmt.test);
                if let Some(msg) = &assert_stmt.msg {
                    self.visit_expr(msg);
                }
                self.guard_stack.push(GuardScope {
                    kind: GuardKind::AssertGuard,
                    expr: expr_str,
                    in_true_branch: true,
                    line,
                });
            }
            Stmt::Delete(del_stmt) => {
                self.in_mutation_target += 1;
                for t in &del_stmt.targets {
                    self.visit_expr(t);
                }
                self.in_mutation_target -= 1;
            }
            Stmt::Expr(expr_stmt) => {
                self.visit_expr(&expr_stmt.value);
            }
            Stmt::TypeAlias(ta) => {
                self.visit_expr(&ta.value);
            }
            Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::IpyEscapeCommand(_) => {}
        }
    }

    fn visit_assign_target(&mut self, target: &Expr, value: Option<&Expr>) {
        match target {
            Expr::Name(n) => {
                let var = n.id.to_string();
                if let Some(val) = value {
                    if let Some(keys) = extract_dict_literal_keys(val) {
                        self.current_scope_mut()
                            .var_dict_keys
                            .insert(var.clone(), keys);
                    }
                    if let Expr::Call(call) = val {
                        let callee = self.resolve_expr_callee(&call.func);
                        self.current_scope_mut()
                            .var_call_origins
                            .insert(var.clone(), callee.clone());
                        // If the callee looks like a class constructor, record variable type
                        if is_class_like_callee(&callee) || self.local_classes.contains(&callee) {
                            self.current_scope_mut()
                                .var_types
                                .insert(var.clone(), callee.clone());
                        }
                        // Check if this allocates a lock
                        if let Some(lock_kind) = classify_lock_constructor(&callee) {
                            let scope_qname = self.current_scope().qualified_name.clone();
                            let lock_id = if self.scope_stack.len() == 1 {
                                format!("{}.{var}", self.module_name)
                            } else {
                                format!("{scope_qname}.{var}")
                            };
                            if self.scope_stack.len() == 1 {
                                self.module_locks
                                    .insert(var.clone(), (lock_kind, lock_id.clone()));
                            }
                            self.current_scope_mut()
                                .var_locks
                                .insert(var.clone(), (lock_kind, lock_id));
                        }
                        // Check if this allocates a context-managed resource outside `with`
                        if let Some(res_kind) = classify_unowned_resource_alloc(&callee, call) {
                            let (line, col) = self.line_col(call.range().start());
                            self.current_scope_mut()
                                .var_resource_allocs
                                .insert(var, (callee, res_kind, line, col));
                        }
                    }
                }
            }
            Expr::Attribute(attr) => {
                self.visit_expr(&attr.value);
                // Track `self._lock = threading.Lock()` inside `__init__`
                if let Expr::Name(base) = &*attr.value {
                    if base.id.as_str() == "self" {
                        if let Some(enclosing_cls) = self.current_scope().enclosing_class.clone() {
                            if let Some(Expr::Call(call)) = value {
                                let callee = self.resolve_expr_callee(&call.func);
                                if let Some(lock_kind) = classify_lock_constructor(&callee) {
                                    let attr_name = attr.attr.id.to_string();
                                    let lock_id = format!("{enclosing_cls}.{attr_name}");
                                    self.class_attr_locks
                                        .insert((enclosing_cls, attr_name), (lock_kind, lock_id));
                                }
                            }
                        }
                    }
                }
            }
            Expr::Subscript(sub) => {
                // Check for `os.environ["KEY"] = ...` mutation or `d["key"] = ...` dict key mutation
                let base_name = self.resolve_expr_callee(&sub.value);
                if base_name == "os.environ" {
                    if let Some(var_name) = extract_string_literal(&sub.slice) {
                        let (line, col) = self.line_col(sub.range.start());
                        let enclosing = self.current_scope().qualified_name.clone();
                        self.env_accesses.push(EnvAccess {
                            var_name,
                            module: self.module_name.to_string(),
                            enclosing_entity: enclosing,
                            file: self.rel_path.to_string(),
                            line,
                            column: col,
                            default_value: None,
                            cast_type: None,
                            is_mutation: true,
                        });
                    }
                } else if let Expr::Name(n) = &*sub.value {
                    if let Some(key) = extract_string_literal(&sub.slice) {
                        self.current_scope_mut()
                            .guarded_dict_keys
                            .insert((n.id.to_string(), key.clone()));
                        if let Some(keys) = self
                            .current_scope_mut()
                            .var_dict_keys
                            .get_mut(n.id.as_str())
                        {
                            keys.insert(key);
                        }
                    }
                }
                self.in_mutation_target += 1;
                self.visit_expr(&sub.value);
                self.in_mutation_target -= 1;
                self.visit_expr(&sub.slice);
            }
            Expr::Tuple(t) => {
                for elt in &t.elts {
                    self.visit_assign_target(elt, None);
                }
            }
            Expr::List(l) => {
                for elt in &l.elts {
                    self.visit_assign_target(elt, None);
                }
            }
            _ => {
                self.in_mutation_target += 1;
                self.visit_expr(target);
                self.in_mutation_target -= 1;
            }
        }
    }

    fn record_return_facts(&mut self, val: &Expr) {
        let (line, col) = self.line_col(val.range().start());
        // If returning a local or module function reference (e.g. decorator returning `wrapper`),
        // emit a HigherOrderCallback edge so `dead-code` sees the returned closure as reachable.
        self.check_higher_order_arg(val);
        // 1. Check dict returns (literal or tracked local dict variable)
        if let Some(keys) = extract_dict_literal_keys(val) {
            if !keys.is_empty() {
                self.current_scope_mut().dict_returns.push(DictShapeSite {
                    keys: keys.into_iter().collect(),
                    line,
                    column: col,
                });
            }
        } else if let Expr::Name(n) = val {
            if let Some(keys) = self
                .current_scope()
                .var_dict_keys
                .get(n.id.as_str())
                .cloned()
            {
                if !keys.is_empty() {
                    self.current_scope_mut().dict_returns.push(DictShapeSite {
                        keys: keys.into_iter().collect(),
                        line,
                        column: col,
                    });
                }
            }
        }

        // 2. Check unowned resource returns (`return open(...)` or `f = open(...); return f`)
        if let Expr::Call(call) = val {
            let callee = self.resolve_expr_callee(&call.func);
            if let Some(res_kind) = classify_unowned_resource_alloc(&callee, call) {
                self.current_scope_mut()
                    .returned_resources
                    .push(ResourceAllocSite {
                        callee,
                        resource_kind: res_kind,
                        returned_variable: None,
                        line,
                        column: col,
                    });
            }
        } else if let Expr::Name(n) = val {
            let var_name = n.id.as_str();
            if let Some((callee, res_kind, alloc_line, alloc_col)) = self
                .current_scope()
                .var_resource_allocs
                .get(var_name)
                .cloned()
            {
                self.current_scope_mut()
                    .returned_resources
                    .push(ResourceAllocSite {
                        callee,
                        resource_kind: res_kind,
                        returned_variable: Some(var_name.to_string()),
                        line: alloc_line,
                        column: alloc_col,
                    });
            }
        }
    }

    fn visit_class_def(&mut self, cls: &StmtClassDef) {
        let parent_scope = self.current_scope().qualified_name.clone();
        let prefix = if parent_scope.ends_with(".<module>") {
            self.module_name.to_string()
        } else {
            parent_scope.clone()
        };
        let cls_name = cls.name.id.to_string();
        let cls_qname = format!("{prefix}.{cls_name}");
        let local_rel = cls_qname
            .strip_prefix(&format!("{}.", self.module_name))
            .unwrap_or(&cls_qname);
        let cls_usr = format!("py:{}#{local_rel}", self.module_name);

        let mut decorators = Vec::new();
        for dec in &cls.decorator_list {
            let dec_callee = self.resolve_expr_callee(&dec.expression);
            decorators.push(dec_callee.clone());
            self.record_decorator_edge(&dec.expression, &dec_callee, &cls_qname);
            self.visit_expr(&dec.expression);
        }

        let mut bases = Vec::new();
        let mut is_protocol = false;
        let mut is_abc = false;
        for base in cls.bases() {
            let resolved_base = self.resolve_expr_callee(base);
            if resolved_base == "Protocol"
                || resolved_base.ends_with(".Protocol")
                || resolved_base == "typing.Protocol"
                || resolved_base == "typing_extensions.Protocol"
            {
                is_protocol = true;
            }
            if resolved_base == "ABC"
                || resolved_base.ends_with(".ABC")
                || resolved_base == "abc.ABC"
            {
                is_abc = true;
            }
            bases.push(resolved_base);
            self.visit_expr(base);
        }
        for kw in cls.keywords() {
            if kw.arg.as_ref().map(|a| a.id.as_str()) == Some("metaclass") {
                let mc = self.resolve_expr_callee(&kw.value);
                if mc.ends_with("ABCMeta") {
                    is_abc = true;
                }
            }
            self.visit_expr(&kw.value);
        }

        // Extract class schema if dataclass / TypedDict / NamedTuple / Pydantic BaseModel / schema class
        if let Some(schema) = self.extract_class_schema(cls, &cls_qname, &decorators, &bases) {
            self.schemas.push(schema);
        }

        // Pre-register methods of this class as local definitions
        self.scope_stack.push(ScopeFrame {
            qualified_name: cls_qname.clone(),
            enclosing_class: Some(cls_qname.clone()),
            execution_context: ExecutionContext::Sync,
            is_function_like: false,
            var_types: BTreeMap::new(),
            var_locks: BTreeMap::new(),
            var_call_origins: BTreeMap::new(),
            var_resource_allocs: BTreeMap::new(),
            var_dict_keys: BTreeMap::new(),
            local_functions: BTreeMap::new(),
            guarded_dict_keys: BTreeSet::new(),
            param_names: BTreeSet::new(),
            used_params: BTreeSet::new(),
            raises: Vec::new(),
            broad_catches: Vec::new(),
            returned_resources: Vec::new(),
            dict_returns: Vec::new(),
            dict_accesses: Vec::new(),
            returns_optional_or_status: false,
            lambda_counter: 0,
            comp_counter: 0,
        });

        for stmt in &cls.body {
            self.visit_stmt(stmt);
        }

        let cls_frame = self.scope_stack.pop().unwrap();
        let (sl, sc, el, ec, bs, be) = self.span_info(cls.range);
        let is_exported = self.is_exported_name(&cls_name, &None);
        let is_test_ep = self.is_test_module && cls_name.starts_with("Test");
        let is_ep =
            is_exported || is_test_ep || self.is_configured_entry_point(&cls_qname, &cls_name);
        let ep_kind = if is_test_ep {
            Some("pytest".to_string())
        } else if is_exported {
            Some("dunder_all".to_string())
        } else if is_ep {
            Some("configured".to_string())
        } else {
            None
        };

        self.entities.push(EntityNode {
            usr: cls_usr,
            qualified_name: cls_qname.clone(),
            name: cls_name,
            module: self.module_name.to_string(),
            enclosing_class: None,
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            end_line: el,
            end_column: ec,
            byte_start: bs,
            byte_end: be,
            kind: EntityKind::Class,
            execution_context: ExecutionContext::Sync,
            decorators,
            parameters: Vec::new(),
            return_annotation: None,
            bases: bases.clone(),
            mro: {
                let mut m = vec![cls_qname];
                m.extend(bases);
                m
            },
            is_protocol,
            is_abc,
            is_abstractmethod: false,
            is_exported,
            is_entry_point: is_ep,
            entry_point_kind: ep_kind,
            skeleton_hash: 0,
            skeleton_features: Vec::new(),
            normalized_skeleton: String::new(),
            statement_count: cls.body.len() as u32,
            raises: cls_frame.raises,
            broad_catches: cls_frame.broad_catches,
            returned_resources: cls_frame.returned_resources,
            dict_returns: cls_frame.dict_returns,
            dict_accesses: cls_frame.dict_accesses,
            returns_optional_or_status: false,
        });
    }

    fn extract_class_schema(
        &self,
        cls: &StmtClassDef,
        cls_qname: &str,
        decorators: &[String],
        bases: &[String],
    ) -> Option<ClassSchema> {
        let is_dataclass = decorators
            .iter()
            .any(|d| d == "dataclass" || d.ends_with(".dataclass"));
        let is_typed_dict = bases
            .iter()
            .any(|b| b == "TypedDict" || b.ends_with(".TypedDict"));
        let is_named_tuple = bases
            .iter()
            .any(|b| b == "NamedTuple" || b.ends_with(".NamedTuple"));
        let is_pydantic = bases
            .iter()
            .any(|b| b == "BaseModel" || b.ends_with(".BaseModel"));

        let flavor = if is_dataclass {
            SchemaFlavor::Dataclass
        } else if is_typed_dict {
            SchemaFlavor::TypedDict
        } else if is_named_tuple {
            SchemaFlavor::NamedTuple
        } else if is_pydantic {
            SchemaFlavor::PydanticBaseModel
        } else {
            SchemaFlavor::PlainClass
        };

        let mut fields = Vec::new();
        for stmt in &cls.body {
            if let Stmt::AnnAssign(ann) = stmt {
                if let Expr::Name(n) = &*ann.target {
                    let fname = n.id.to_string();
                    if fname.starts_with('_') {
                        continue;
                    }
                    let ty_str = normalize_type_str(self.slice_range(ann.annotation.range()));
                    let def_str = ann
                        .value
                        .as_ref()
                        .map(|v| self.slice_range(v.range()).trim().to_string());
                    let (line, _) = self.line_col(ann.range.start());
                    fields.push(SchemaField {
                        name: fname,
                        type_annotation: ty_str,
                        default_value: def_str,
                        line,
                    });
                }
            }
        }

        if fields.is_empty() && flavor == SchemaFlavor::PlainClass {
            return None;
        }
        // Only record PlainClass schemas if they have at least 2 annotated fields
        if flavor == SchemaFlavor::PlainClass && fields.len() < 2 {
            return None;
        }

        let (line, col) = self.line_col(cls.range.start());
        Some(ClassSchema {
            qualified_name: cls_qname.to_string(),
            name: cls.name.id.to_string(),
            module: self.module_name.to_string(),
            file: self.rel_path.to_string(),
            line,
            column: col,
            flavor,
            fields,
        })
    }

    fn visit_function_def(&mut self, func: &StmtFunctionDef) {
        let parent_scope = self.current_scope().qualified_name.clone();
        let enclosing_cls = self.current_scope().enclosing_class.clone();
        let prefix = if parent_scope.ends_with(".<module>") {
            self.module_name.to_string()
        } else {
            parent_scope.clone()
        };
        let fn_name = func.name.id.to_string();
        let fn_qname = format!("{prefix}.{fn_name}");
        let local_rel = fn_qname
            .strip_prefix(&format!("{}.", self.module_name))
            .unwrap_or(&fn_qname);
        let fn_usr = format!("py:{}#{local_rel}", self.module_name);

        // Register in enclosing function/module scope so calls inside the enclosing scope resolve
        self.current_scope_mut()
            .local_functions
            .insert(fn_name.clone(), fn_qname.clone());

        let mut decorators = Vec::new();
        let mut is_staticmethod = false;
        let mut is_classmethod = false;
        let mut is_abstractmethod = false;
        let mut decorator_ep_kind: Option<String> = None;

        for dec in &func.decorator_list {
            let dec_callee = self.resolve_expr_callee(&dec.expression);
            if dec_callee == "staticmethod" || dec_callee.ends_with(".staticmethod") {
                is_staticmethod = true;
            }
            if dec_callee == "classmethod" || dec_callee.ends_with(".classmethod") {
                is_classmethod = true;
            }
            if dec_callee == "abstractmethod"
                || dec_callee.ends_with(".abstractmethod")
                || dec_callee == "overload"
                || dec_callee.ends_with(".overload")
            {
                is_abstractmethod = true;
            }
            if is_route_or_cli_decorator(&dec_callee) {
                decorator_ep_kind = Some(
                    if dec_callee.contains("command") || dec_callee.contains("cli") {
                        "cli_decorator".to_string()
                    } else {
                        "route_decorator".to_string()
                    },
                );
            }
            decorators.push(dec_callee.clone());
            self.record_decorator_edge(&dec.expression, &dec_callee, &fn_qname);
            self.visit_expr(&dec.expression);
        }

        let is_method = enclosing_cls.is_some();
        let kind = match (is_method, func.is_async) {
            (false, false) => EntityKind::Function,
            (false, true) => EntityKind::AsyncFunction,
            (true, false) => EntityKind::Method,
            (true, true) => EntityKind::AsyncMethod,
        };
        let exec_ctx = if func.is_async {
            ExecutionContext::Async
        } else if is_staticmethod {
            ExecutionContext::StaticMethod
        } else if is_classmethod {
            ExecutionContext::ClassMethod
        } else {
            ExecutionContext::Sync
        };

        let mut var_types = BTreeMap::new();
        if let Some(cls) = &enclosing_cls {
            var_types.insert("self".to_string(), cls.clone());
            var_types.insert("cls".to_string(), cls.clone());
        }

        let mut param_names = BTreeSet::new();
        let mut raw_params = Vec::new();

        for (p_kind, pwd) in func
            .parameters
            .posonlyargs
            .iter()
            .map(|p| (ParamKind::PosOnly, p))
            .chain(
                func.parameters
                    .args
                    .iter()
                    .map(|p| (ParamKind::Positional, p)),
            )
            .chain(
                func.parameters
                    .kwonlyargs
                    .iter()
                    .map(|p| (ParamKind::KwOnly, p)),
            )
        {
            let pname = pwd.parameter.name.id.to_string();
            let ann = pwd.parameter.annotation.as_ref().map(|a| {
                self.visit_expr(a);
                let resolved = self.resolve_expr_callee(a);
                var_types.insert(pname.clone(), resolved);
                self.slice_range(a.range()).trim().to_string()
            });
            let def_val = pwd.default.as_ref().map(|d| {
                self.visit_expr(d);
                self.slice_range(d.range()).trim().to_string()
            });
            let (pline, pcol) = self.line_col(pwd.parameter.range.start());
            param_names.insert(pname.clone());
            raw_params.push(ParamInfo {
                name: pname,
                kind: p_kind,
                annotation: ann,
                default_value: def_val,
                used_in_body: false,
                line: pline,
                column: pcol,
            });
        }
        if let Some(vararg) = &func.parameters.vararg {
            let pname = vararg.name.id.to_string();
            let ann = vararg.annotation.as_ref().map(|a| {
                self.visit_expr(a);
                self.slice_range(a.range()).trim().to_string()
            });
            let (pline, pcol) = self.line_col(vararg.range.start());
            param_names.insert(pname.clone());
            raw_params.push(ParamInfo {
                name: pname,
                kind: ParamKind::VarArg,
                annotation: ann,
                default_value: None,
                used_in_body: false,
                line: pline,
                column: pcol,
            });
        }
        if let Some(kwarg) = &func.parameters.kwarg {
            let pname = kwarg.name.id.to_string();
            let ann = kwarg.annotation.as_ref().map(|a| {
                self.visit_expr(a);
                self.slice_range(a.range()).trim().to_string()
            });
            let (pline, pcol) = self.line_col(kwarg.range.start());
            param_names.insert(pname.clone());
            raw_params.push(ParamInfo {
                name: pname,
                kind: ParamKind::KwArg,
                annotation: ann,
                default_value: None,
                used_in_body: false,
                line: pline,
                column: pcol,
            });
        }

        let return_annotation = func.returns.as_ref().map(|r| {
            self.visit_expr(r);
            self.slice_range(r.range()).trim().to_string()
        });

        // Pre-scan nested function definitions inside `func.body` so forward references between
        // sibling nested functions resolve cleanly.
        let mut local_functions = BTreeMap::new();
        for stmt in &func.body {
            if let Stmt::FunctionDef(inner_f) = stmt {
                let inner_name = inner_f.name.id.to_string();
                local_functions.insert(inner_name.clone(), format!("{fn_qname}.{inner_name}"));
            }
        }

        self.scope_stack.push(ScopeFrame {
            qualified_name: fn_qname.clone(),
            enclosing_class: enclosing_cls.clone(),
            execution_context: exec_ctx,
            is_function_like: true,
            var_types,
            var_locks: BTreeMap::new(),
            var_call_origins: BTreeMap::new(),
            var_resource_allocs: BTreeMap::new(),
            var_dict_keys: BTreeMap::new(),
            local_functions,
            guarded_dict_keys: BTreeSet::new(),
            param_names,
            used_params: BTreeSet::new(),
            raises: Vec::new(),
            broad_catches: Vec::new(),
            returned_resources: Vec::new(),
            dict_returns: Vec::new(),
            dict_accesses: Vec::new(),
            returns_optional_or_status: return_annotation
                .as_deref()
                .is_some_and(|r| r.contains("None") || r.contains("Optional") || r == "bool"),
            lambda_counter: 0,
            comp_counter: 0,
        });

        for stmt in &func.body {
            self.visit_stmt(stmt);
        }

        let fn_frame = self.scope_stack.pop().unwrap();
        // Propagate used outer parameters to enclosing function scopes (closures)
        for u in &fn_frame.used_params {
            if self.current_scope().param_names.contains(u) {
                self.current_scope_mut().used_params.insert(u.clone());
            }
        }
        let is_stub = is_stub_function_body(&func.body);
        for p in &mut raw_params {
            p.used_in_body =
                fn_frame.used_params.contains(&p.name) || p.name == "self" || p.name == "cls";
        }

        let (skel_hash, skel_features, norm_skel, stmt_count) = compute_function_skeleton(func);
        let (sl, sc, el, ec, bs, be) = self.span_info(func.range);

        let is_exported = self.is_exported_name(&fn_name, &enclosing_cls);
        let is_configured_ep = self.is_configured_entry_point(&fn_qname, &fn_name);
        let is_skill_ep = self.is_skill_script_entry_point(&fn_qname);
        let is_test_ep = self.is_test_module
            && (fn_name.starts_with("test_")
                || fn_name == "setUp"
                || fn_name == "tearDown"
                || fn_name.starts_with("pytest_"));
        let is_dunder_protocol = is_magic_dunder_method(&fn_name);
        let is_main_guard_fn =
            enclosing_cls.is_none() && fn_name == "main" && (self.has_main_guard || is_skill_ep);

        let is_ep = is_exported
            || is_configured_ep
            || is_skill_ep
            || is_main_guard_fn
            || is_test_ep
            || decorator_ep_kind.is_some()
            || is_dunder_protocol;
        let ep_kind = if is_skill_ep || is_main_guard_fn {
            Some("main_guard".to_string())
        } else if is_configured_ep {
            Some("pyproject_script".to_string())
        } else if decorator_ep_kind.is_some() {
            decorator_ep_kind
        } else if is_test_ep {
            Some("pytest".to_string())
        } else if is_exported {
            Some("dunder_all".to_string())
        } else if is_dunder_protocol {
            Some("dunder_protocol".to_string())
        } else {
            None
        };

        self.entities.push(EntityNode {
            usr: fn_usr,
            qualified_name: fn_qname,
            name: fn_name,
            module: self.module_name.to_string(),
            enclosing_class: enclosing_cls,
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            end_line: el,
            end_column: ec,
            byte_start: bs,
            byte_end: be,
            kind,
            execution_context: exec_ctx,
            decorators,
            parameters: raw_params,
            return_annotation,
            bases: Vec::new(),
            mro: Vec::new(),
            is_protocol: false,
            is_abc: false,
            is_abstractmethod: is_abstractmethod || is_stub,
            is_exported,
            is_entry_point: is_ep,
            entry_point_kind: ep_kind,
            skeleton_hash: skel_hash,
            skeleton_features: skel_features,
            normalized_skeleton: norm_skel,
            statement_count: stmt_count,
            raises: fn_frame.raises,
            broad_catches: fn_frame.broad_catches,
            returned_resources: fn_frame.returned_resources,
            dict_returns: fn_frame.dict_returns,
            dict_accesses: fn_frame.dict_accesses,
            returns_optional_or_status: fn_frame.returns_optional_or_status,
        });
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                let raw_callee = self.slice_range(call.func.range()).trim().to_string();
                let resolved_callee = self.resolve_expr_callee(&call.func);

                // Check for type cast wrapping env access (`int(os.getenv(...))`)
                let prev_cast = self.cast_context.take();
                if matches!(
                    raw_callee.as_str(),
                    "int" | "float" | "bool" | "str" | "Path"
                ) {
                    self.cast_context = Some(raw_callee.clone());
                }

                self.record_call_site(call, &raw_callee, &resolved_callee);

                // Await applies to this call, not to synchronous argument evaluation.
                let enclosing_await_depth = self.await_depth;
                self.await_depth = 0;
                // Visit callee and arguments
                self.visit_expr(&call.func);
                for arg in &call.arguments.args {
                    self.visit_expr(arg);
                    self.check_higher_order_arg(arg);
                }
                for kw in &call.arguments.keywords {
                    self.visit_expr(&kw.value);
                    self.check_higher_order_arg(&kw.value);
                }
                self.cast_context = prev_cast;
                self.await_depth = enclosing_await_depth;
            }
            Expr::Await(aw) => {
                // Check if awaiting while holding a threading.Lock
                let (line, col) = self.line_col(aw.range.start());
                for ws in &self.with_stack {
                    if ws.resource_kind == ResourceKind::ThreadingLock {
                        if let Some(lid) = &ws.lock_identity {
                            let caller = self.current_scope().qualified_name.clone();
                            self.await_under_threading_lock
                                .push((caller, lid.clone(), line, col));
                        }
                    }
                }
                self.await_depth += 1;
                self.visit_expr(&aw.value);
                self.await_depth -= 1;
            }
            Expr::Subscript(sub) => {
                let base_resolved = self.resolve_expr_callee(&sub.value);
                if base_resolved == "os.environ" {
                    if self.in_mutation_target == 0 && sub.ctx.is_load() {
                        if let Some(var_name) = extract_string_literal(&sub.slice) {
                            let (line, col) = self.line_col(sub.range.start());
                            let enclosing = self.current_scope().qualified_name.clone();
                            let cast = self.cast_context.clone();
                            self.env_accesses.push(EnvAccess {
                                var_name,
                                module: self.module_name.to_string(),
                                enclosing_entity: enclosing,
                                file: self.rel_path.to_string(),
                                line,
                                column: col,
                                default_value: None,
                                cast_type: cast,
                                is_mutation: false,
                            });
                        }
                    }
                } else if self.in_mutation_target == 0 && sub.ctx.is_load() {
                    if let Some(key) = extract_string_literal(&sub.slice) {
                        // Check if base is a variable holding a call result or a parameter
                        if let Expr::Name(n) = &*sub.value {
                            let vname = n.id.as_str();
                            if !self.is_dict_key_guarded(vname, &key) {
                                let (line, col) = self.line_col(sub.range.start());
                                let origin =
                                    self.current_scope().var_call_origins.get(vname).cloned();
                                let is_param = self.current_scope().param_names.contains(vname);
                                if origin.is_some() || is_param {
                                    self.current_scope_mut().dict_accesses.push(DictAccessSite {
                                        source_callee: origin,
                                        param_name: if is_param {
                                            Some(vname.to_string())
                                        } else {
                                            None
                                        },
                                        key,
                                        is_required_subscript: true,
                                        line,
                                        column: col,
                                    });
                                }
                            }
                        } else if let Expr::Call(inner_call) = &*sub.value {
                            let origin = self.resolve_expr_callee(&inner_call.func);
                            let (line, col) = self.line_col(sub.range.start());
                            self.current_scope_mut().dict_accesses.push(DictAccessSite {
                                source_callee: Some(origin),
                                param_name: None,
                                key,
                                is_required_subscript: true,
                                line,
                                column: col,
                            });
                        }
                    }
                }
                self.visit_expr(&sub.value);
                self.visit_expr(&sub.slice);
            }
            Expr::Name(n) => {
                let id = n.id.as_str();
                if self.current_scope().param_names.contains(id) {
                    self.current_scope_mut().used_params.insert(id.to_string());
                }
            }
            Expr::Lambda(lam) => {
                let idx = self.current_scope().lambda_counter;
                self.current_scope_mut().lambda_counter += 1;
                let parent_qname = self.current_scope().qualified_name.clone();
                let lam_name = format!("<lambda#{idx}>");
                let lam_qname = format!("{parent_qname}.{lam_name}");
                let local_rel = lam_qname
                    .strip_prefix(&format!("{}.", self.module_name))
                    .unwrap_or(&lam_qname);
                let lam_usr = format!("py:{}#{local_rel}", self.module_name);
                let (sl, sc, el, ec, bs, be) = self.span_info(lam.range);

                let mut param_names = BTreeSet::new();
                if let Some(params) = &lam.parameters {
                    for p in params.iter_non_variadic_params() {
                        param_names.insert(p.parameter.name.id.to_string());
                    }
                }
                let inherited_vars = self.current_scope().var_types.clone();
                self.scope_stack.push(ScopeFrame {
                    qualified_name: lam_qname.clone(),
                    enclosing_class: self.current_scope().enclosing_class.clone(),
                    execution_context: ExecutionContext::Sync,
                    is_function_like: true,
                    var_types: inherited_vars,
                    var_locks: BTreeMap::new(),
                    var_call_origins: BTreeMap::new(),
                    var_resource_allocs: BTreeMap::new(),
                    var_dict_keys: BTreeMap::new(),
                    local_functions: BTreeMap::new(),
                    guarded_dict_keys: BTreeSet::new(),
                    param_names,
                    used_params: BTreeSet::new(),
                    raises: Vec::new(),
                    broad_catches: Vec::new(),
                    returned_resources: Vec::new(),
                    dict_returns: Vec::new(),
                    dict_accesses: Vec::new(),
                    returns_optional_or_status: false,
                    lambda_counter: 0,
                    comp_counter: 0,
                });
                self.visit_expr(&lam.body);
                let lam_frame = self.scope_stack.pop().unwrap();
                for u in &lam_frame.used_params {
                    if self.current_scope().param_names.contains(u) {
                        self.current_scope_mut().used_params.insert(u.clone());
                    }
                }
                self.entities.push(EntityNode {
                    usr: lam_usr,
                    qualified_name: lam_qname,
                    name: lam_name,
                    module: self.module_name.to_string(),
                    enclosing_class: None,
                    file: self.rel_path.to_string(),
                    line: sl,
                    column: sc,
                    end_line: el,
                    end_column: ec,
                    byte_start: bs,
                    byte_end: be,
                    kind: EntityKind::Lambda,
                    execution_context: ExecutionContext::Sync,
                    decorators: Vec::new(),
                    parameters: Vec::new(),
                    return_annotation: None,
                    bases: Vec::new(),
                    mro: Vec::new(),
                    is_protocol: false,
                    is_abc: false,
                    is_abstractmethod: false,
                    is_exported: false,
                    is_entry_point: false,
                    entry_point_kind: None,
                    skeleton_hash: 0,
                    skeleton_features: Vec::new(),
                    normalized_skeleton: String::new(),
                    statement_count: 1,
                    raises: lam_frame.raises,
                    broad_catches: lam_frame.broad_catches,
                    returned_resources: lam_frame.returned_resources,
                    dict_returns: lam_frame.dict_returns,
                    dict_accesses: lam_frame.dict_accesses,
                    returns_optional_or_status: false,
                });
            }
            Expr::Yield(y) => {
                if let Some(val) = &y.value {
                    self.visit_expr(val);
                    self.record_return_facts(val);
                }
            }
            Expr::YieldFrom(yf) => {
                self.visit_expr(&yf.value);
            }
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.visit_expr(v);
                }
            }
            Expr::Named(n) => {
                self.visit_expr(&n.value);
                self.visit_assign_target(&n.target, Some(&n.value));
            }
            Expr::BinOp(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::UnaryOp(u) => {
                self.visit_expr(&u.operand);
            }
            Expr::If(if_e) => {
                let (line, _) = self.line_col(if_e.range.start());
                let cond_str = self.slice_range(if_e.test.range()).trim().to_string();
                let guard_kind = classify_guard(&if_e.test, &cond_str);
                self.visit_expr(&if_e.test);
                self.guard_stack.push(GuardScope {
                    kind: guard_kind,
                    expr: cond_str.clone(),
                    in_true_branch: true,
                    line,
                });
                self.visit_expr(&if_e.body);
                self.guard_stack.pop();
                self.guard_stack.push(GuardScope {
                    kind: guard_kind,
                    expr: cond_str,
                    in_true_branch: false,
                    line,
                });
                self.visit_expr(&if_e.orelse);
                self.guard_stack.pop();
            }
            Expr::Dict(d) => {
                for item in &d.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&item.value);
                }
            }
            Expr::Set(s) => {
                for e in &s.elts {
                    self.visit_expr(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.visit_expr(e);
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.visit_expr(e);
                }
            }
            Expr::ListComp(c) => {
                for g in &c.generators {
                    self.visit_expr(&g.iter);
                    for if_e in &g.ifs {
                        self.visit_expr(if_e);
                    }
                }
                self.visit_expr(&c.elt);
            }
            Expr::SetComp(c) => {
                for g in &c.generators {
                    self.visit_expr(&g.iter);
                    for if_e in &g.ifs {
                        self.visit_expr(if_e);
                    }
                }
                self.visit_expr(&c.elt);
            }
            Expr::DictComp(c) => {
                for g in &c.generators {
                    self.visit_expr(&g.iter);
                    for if_e in &g.ifs {
                        self.visit_expr(if_e);
                    }
                }
                if let Some(k) = &c.key {
                    self.visit_expr(k);
                }
                self.visit_expr(&c.value);
            }
            Expr::Generator(c) => {
                for g in &c.generators {
                    self.visit_expr(&g.iter);
                    for if_e in &g.ifs {
                        self.visit_expr(if_e);
                    }
                }
                self.visit_expr(&c.elt);
            }
            Expr::Compare(cmp) => {
                for op in &cmp.operands {
                    self.visit_expr(op);
                }
            }
            Expr::Attribute(attr) => {
                self.visit_expr(&attr.value);
            }
            Expr::Starred(st) => {
                self.visit_expr(&st.value);
            }
            Expr::Slice(sl) => {
                if let Some(l) = &sl.lower {
                    self.visit_expr(l);
                }
                if let Some(u) = &sl.upper {
                    self.visit_expr(u);
                }
                if let Some(s) = &sl.step {
                    self.visit_expr(s);
                }
            }
            Expr::FString(string) => {
                for element in string.value.elements() {
                    self.visit_interpolation(element);
                }
            }
            Expr::TString(string) => {
                for element in string.value.elements() {
                    self.visit_interpolation(element);
                }
            }
            Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::Constant(_)
            | Expr::IpyEscapeCommand(_) => {}
        }
    }

    fn visit_interpolation(&mut self, element: &ruff_python_ast::InterpolatedStringElement) {
        if let ruff_python_ast::InterpolatedStringElement::Interpolation(interpolation) = element {
            self.visit_expr(&interpolation.expression);
            if let Some(spec) = &interpolation.format_spec {
                for nested in &spec.elements {
                    self.visit_interpolation(nested);
                }
            }
        }
    }

    fn check_higher_order_arg(&mut self, arg: &Expr) {
        if let Expr::Name(n) = arg {
            let name = n.id.as_str();
            let local_fn_target = self
                .scope_stack
                .iter()
                .rev()
                .find_map(|s| s.local_functions.get(name).cloned());
            if let Some(target) = local_fn_target {
                if !self.local_classes.contains(name) {
                    self.emit_edge(
                        arg.range(),
                        name,
                        &target,
                        CallEdgeKind::HigherOrderCallback,
                        EdgeConfidence::Plausible,
                        Vec::new(),
                        Vec::new(),
                    );
                }
            } else if self.local_defs.contains(name) && !self.local_classes.contains(name) {
                let target = format!("{}.{name}", self.module_name);
                self.emit_edge(
                    arg.range(),
                    name,
                    &target,
                    CallEdgeKind::HigherOrderCallback,
                    EdgeConfidence::Plausible,
                    Vec::new(),
                    Vec::new(),
                );
            } else if let Some((mod_target, Some(sym))) = self.import_bindings.get(name).cloned() {
                let target = if mod_target.is_empty() {
                    sym
                } else {
                    format!("{mod_target}.{sym}")
                };
                self.emit_edge(
                    arg.range(),
                    name,
                    &target,
                    CallEdgeKind::HigherOrderCallback,
                    EdgeConfidence::Plausible,
                    Vec::new(),
                    Vec::new(),
                );
            }
        }
    }

    fn record_decorator_edge(&mut self, expr: &Expr, resolved_callee: &str, decorated_qname: &str) {
        let raw = self.slice_range(expr.range()).trim().to_string();
        let (sl, sc, _, _, bs, be) = self.span_info(expr.range());
        let ctx_id = self.contexts.len() as u32;
        let exec_ctx = self.current_scope().execution_context;
        self.contexts.push(CallSiteContext {
            context_id: ctx_id,
            caller: decorated_qname.to_string(),
            callee: resolved_callee.to_string(),
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            execution_context: exec_ctx,
            is_awaited: false,
            try_scopes: self.try_stack.clone(),
            with_scopes: self.with_stack.clone(),
            guards: self.guard_stack.clone(),
        });
        self.calls.push(CallEdge {
            caller: decorated_qname.to_string(),
            callee: resolved_callee.to_string(),
            callee_raw: raw,
            module: self.module_name.to_string(),
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            byte_start: bs,
            byte_end: be,
            kind: CallEdgeKind::Decorator,
            confidence: EdgeConfidence::Proven,
            execution_context: exec_ctx,
            is_awaited: false,
            arg_constants: Vec::new(),
            passed_dict_keys: Vec::new(),
            context_id: ctx_id,
        });
    }

    fn record_call_site(
        &mut self,
        call: &ruff_python_ast::ExprCall,
        raw_callee: &str,
        resolved_callee: &str,
    ) {
        let (line, col) = self.line_col(call.range().start());

        // 1. Check for env accesses (`os.getenv`, `os.environ.get`, `os.environ.setdefault`, `os.putenv`)
        if matches!(
            resolved_callee,
            "os.getenv" | "os.environ.get" | "os.environ.setdefault" | "os.putenv"
        ) {
            if let Some(first_arg) = call.arguments.args.first() {
                if let Some(var_name) = extract_string_literal(first_arg) {
                    let def_val = call
                        .arguments
                        .args
                        .get(1)
                        .or_else(|| {
                            call.arguments
                                .keywords
                                .iter()
                                .find(|k| k.arg.as_ref().map(|a| a.id.as_str()) == Some("default"))
                                .map(|k| &k.value)
                        })
                        .map(|e| self.slice_range(e.range()).trim().to_string());
                    let enclosing = self.current_scope().qualified_name.clone();
                    let is_mut = resolved_callee == "os.environ.setdefault"
                        || resolved_callee == "os.putenv";
                    self.env_accesses.push(EnvAccess {
                        var_name,
                        module: self.module_name.to_string(),
                        enclosing_entity: enclosing,
                        file: self.rel_path.to_string(),
                        line,
                        column: col,
                        default_value: def_val,
                        cast_type: self.cast_context.clone(),
                        is_mutation: is_mut,
                    });
                }
            }
        }

        // 2. Check for `d.get("key")` on call result or parameter
        if let Expr::Attribute(attr) = &*call.func {
            if attr.attr.id.as_str() == "get" {
                if let Some(first_arg) = call.arguments.args.first() {
                    if let Some(key) = extract_string_literal(first_arg) {
                        if let Expr::Name(n) = &*attr.value {
                            let vname = n.id.as_str();
                            let origin = self.current_scope().var_call_origins.get(vname).cloned();
                            let is_param = self.current_scope().param_names.contains(vname);
                            if origin.is_some() || is_param {
                                self.current_scope_mut().dict_accesses.push(DictAccessSite {
                                    source_callee: origin,
                                    param_name: if is_param {
                                        Some(vname.to_string())
                                    } else {
                                        None
                                    },
                                    key,
                                    is_required_subscript: false,
                                    line,
                                    column: col,
                                });
                            }
                        }
                    }
                }
            }
        }

        // 3. Check for known raising stdlib/3p calls
        if let Some(raised_exc) = classify_known_raising_call(resolved_callee, call) {
            let is_caught = self.is_exception_caught_locally(&raised_exc);
            self.current_scope_mut().raises.push(RaiseSite {
                exception_type: raised_exc,
                message_expr: Some(raw_callee.to_string()),
                is_explicit: false,
                is_caught_locally: is_caught,
                line,
                column: col,
            });
        }

        // 4. Extract constant arguments & passed dict literal keys
        let mut arg_constants = Vec::new();
        let mut passed_dict_keys = Vec::new();
        for (idx, arg) in call.arguments.args.iter().enumerate() {
            let key_id = idx.to_string();
            let const_repr = extract_constant_repr(arg);
            arg_constants.push((key_id.clone(), const_repr));
            if let Some(keys) = extract_dict_literal_keys(arg) {
                passed_dict_keys.push((key_id, keys.into_iter().collect()));
            } else if let Expr::Name(n) = arg {
                if let Some(keys) = self.current_scope().var_dict_keys.get(n.id.as_str()) {
                    passed_dict_keys.push((key_id, keys.iter().cloned().collect()));
                }
            }
        }
        for kw in &call.arguments.keywords {
            if let Some(arg_name) = &kw.arg {
                let key_id = arg_name.id.to_string();
                let const_repr = extract_constant_repr(&kw.value);
                arg_constants.push((key_id.clone(), const_repr));
                if let Some(keys) = extract_dict_literal_keys(&kw.value) {
                    passed_dict_keys.push((key_id, keys.into_iter().collect()));
                } else if let Expr::Name(n) = &kw.value {
                    if let Some(keys) = self.current_scope().var_dict_keys.get(n.id.as_str()) {
                        passed_dict_keys.push((key_id, keys.iter().cloned().collect()));
                    }
                }
            }
        }

        // 5. Classify call edge kind and confidence
        let (kind, confidence) = match &*call.func {
            Expr::Attribute(attr) => {
                if is_super_call(&attr.value) {
                    (CallEdgeKind::SuperCall, EdgeConfidence::Proven)
                } else if let Expr::Name(base) = &*attr.value {
                    let bname = base.id.as_str();
                    if bname == "self" || bname == "cls" {
                        (CallEdgeKind::BoundMethod, EdgeConfidence::Proven)
                    } else if self.import_bindings.contains_key(bname) {
                        (CallEdgeKind::Direct, EdgeConfidence::Proven)
                    } else if self.current_scope().var_types.contains_key(bname) {
                        (CallEdgeKind::DynamicDispatch, EdgeConfidence::Plausible)
                    } else {
                        (CallEdgeKind::BoundMethod, EdgeConfidence::Plausible)
                    }
                } else {
                    (CallEdgeKind::BoundMethod, EdgeConfidence::Plausible)
                }
            }
            _ => {
                if self.local_classes.contains(raw_callee) || is_class_like_callee(resolved_callee)
                {
                    (CallEdgeKind::Constructor, EdgeConfidence::Proven)
                } else {
                    (CallEdgeKind::Direct, EdgeConfidence::Proven)
                }
            }
        };

        self.emit_edge(
            call.range(),
            raw_callee,
            resolved_callee,
            kind,
            confidence,
            arg_constants,
            passed_dict_keys,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_edge(
        &mut self,
        range: TextRange,
        raw_callee: &str,
        resolved_callee: &str,
        kind: CallEdgeKind,
        confidence: EdgeConfidence,
        arg_constants: Vec<(String, Option<String>)>,
        passed_dict_keys: Vec<(String, Vec<String>)>,
    ) {
        let (sl, sc, _, _, bs, be) = self.span_info(range);
        let caller = self.current_scope().qualified_name.clone();
        let exec_ctx = self.current_scope().execution_context;
        let is_awaited = self.await_depth > 0;
        let ctx_id = self.contexts.len() as u32;

        self.contexts.push(CallSiteContext {
            context_id: ctx_id,
            caller: caller.clone(),
            callee: resolved_callee.to_string(),
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            execution_context: exec_ctx,
            is_awaited,
            try_scopes: self.try_stack.clone(),
            with_scopes: self.with_stack.clone(),
            guards: self.guard_stack.clone(),
        });

        self.calls.push(CallEdge {
            caller,
            callee: resolved_callee.to_string(),
            callee_raw: raw_callee.to_string(),
            module: self.module_name.to_string(),
            file: self.rel_path.to_string(),
            line: sl,
            column: sc,
            byte_start: bs,
            byte_end: be,
            kind,
            confidence,
            execution_context: exec_ctx,
            is_awaited,
            arg_constants,
            passed_dict_keys,
            context_id: ctx_id,
        });
    }

    fn resolve_expr_callee(&self, expr: &Expr) -> String {
        match expr {
            Expr::Name(n) => {
                let id = n.id.as_str();
                for scope in self.scope_stack.iter().rev() {
                    if let Some(qname) = scope.local_functions.get(id) {
                        return qname.clone();
                    }
                }
                if let Some((target_mod, sym_opt)) = self.import_bindings.get(id) {
                    if let Some(sym) = sym_opt {
                        if target_mod.is_empty() {
                            return sym.clone();
                        }
                        return format!("{target_mod}.{sym}");
                    }
                    return target_mod.clone();
                }
                if self.local_defs.contains(id) {
                    return format!("{}.{id}", self.module_name);
                }
                id.to_string()
            }
            Expr::Attribute(attr) => {
                let attr_name = attr.attr.id.as_str();
                if is_super_call(&attr.value) {
                    if let Some(cls) = &self.current_scope().enclosing_class {
                        return format!("super({cls}).{attr_name}");
                    }
                    return format!("super.{attr_name}");
                }
                if let Expr::Name(base) = &*attr.value {
                    let bname = base.id.as_str();
                    if bname == "self" || bname == "cls" {
                        if let Some(cls) = &self.current_scope().enclosing_class {
                            return format!("{cls}.{attr_name}");
                        }
                    }
                    if let Some(var_ty) = self.current_scope().var_types.get(bname) {
                        return format!("{var_ty}.{attr_name}");
                    }
                    if let Some((target_mod, sym_opt)) = self.import_bindings.get(bname) {
                        if let Some(sym) = sym_opt {
                            if target_mod.is_empty() {
                                return format!("{sym}.{attr_name}");
                            }
                            return format!("{target_mod}.{sym}.{attr_name}");
                        }
                        return format!("{target_mod}.{attr_name}");
                    }
                    if self.local_classes.contains(bname) {
                        return format!("{}.{bname}.{attr_name}", self.module_name);
                    }
                }
                let prefix = self.resolve_expr_callee(&attr.value);
                format!("{prefix}.{attr_name}")
            }
            Expr::Call(c) => self.resolve_expr_callee(&c.func),
            Expr::Subscript(s) => self.resolve_expr_callee(&s.value),
            _ => self.slice_range(expr.range()).trim().to_string(),
        }
    }

    fn classify_with_resource(
        &self,
        expr: &Expr,
        ctx_str: &str,
        is_async: bool,
    ) -> (ResourceKind, Option<String>) {
        // 1. Variable reference (`with lock_a:` or `with self._lock:`)
        if let Expr::Name(n) = expr {
            let vname = n.id.as_str();
            if let Some((rk, lid)) = self.current_scope().var_locks.get(vname) {
                return (*rk, Some(lid.clone()));
            }
            if let Some((rk, lid)) = self.module_locks.get(vname) {
                return (*rk, Some(lid.clone()));
            }
            if let Some((target_mod, Some(sym))) = self.import_bindings.get(vname) {
                let lower = sym.to_ascii_lowercase();
                if lower.contains("lock") || lower.contains("mutex") {
                    let rk = if is_async {
                        ResourceKind::AsyncioLock
                    } else {
                        ResourceKind::ThreadingLock
                    };
                    return (rk, Some(format!("{target_mod}.{sym}")));
                }
            }
            let lower = vname.to_ascii_lowercase();
            if lower.contains("lock") || lower.contains("mutex") {
                let rk = if is_async {
                    ResourceKind::AsyncioLock
                } else {
                    ResourceKind::ThreadingLock
                };
                return (rk, Some(format!("{}.{vname}", self.module_name)));
            }
        }
        if let Expr::Attribute(attr) = expr {
            let attr_name = attr.attr.id.as_str();
            if let Expr::Name(base) = &*attr.value {
                if base.id.as_str() == "self" {
                    if let Some(cls) = &self.current_scope().enclosing_class {
                        if let Some((rk, lid)) = self
                            .class_attr_locks
                            .get(&(cls.clone(), attr_name.to_string()))
                        {
                            return (*rk, Some(lid.clone()));
                        }
                        let lower = attr_name.to_ascii_lowercase();
                        if lower.contains("lock") || lower.contains("mutex") {
                            let rk = if is_async {
                                ResourceKind::AsyncioLock
                            } else {
                                ResourceKind::ThreadingLock
                            };
                            return (rk, Some(format!("{cls}.{attr_name}")));
                        }
                    }
                }
            }
            let resolved = self.resolve_expr_callee(expr);
            let lower = attr_name.to_ascii_lowercase();
            if lower.contains("lock") || lower.contains("mutex") {
                let rk = if is_async {
                    ResourceKind::AsyncioLock
                } else {
                    ResourceKind::ThreadingLock
                };
                return (rk, Some(resolved));
            }
        }
        // 2. Call expression (`with open(...):`, `with tempfile.TemporaryDirectory():`, `with db.transaction():`)
        if let Expr::Call(call) = expr {
            let callee = self.resolve_expr_callee(&call.func);
            if let Some(rk) = classify_lock_constructor(&callee) {
                return (rk, Some(callee));
            }
            if callee == "open" || callee.ends_with(".open") {
                return (ResourceKind::FileHandle, None);
            }
            if callee.contains("Temp") || callee.contains("temp") {
                return (ResourceKind::Tempfile, None);
            }
            if callee.contains("Client") || callee.contains("Session") {
                return (ResourceKind::HttpSession, None);
            }
            if callee.contains("transaction") || callee.contains("atomic") {
                return (ResourceKind::Transaction, None);
            }
            if callee.contains("Popen") {
                return (ResourceKind::Subprocess, None);
            }
        }
        if ctx_str.contains("lock") || ctx_str.contains("Lock") {
            let rk = if is_async {
                ResourceKind::AsyncioLock
            } else {
                ResourceKind::ThreadingLock
            };
            return (rk, Some(ctx_str.to_string()));
        }
        (ResourceKind::Other, None)
    }

    fn extract_exception_type(&self, expr: &Expr) -> String {
        match expr {
            Expr::Call(c) => self.resolve_expr_callee(&c.func),
            Expr::Name(_) | Expr::Attribute(_) => self.resolve_expr_callee(expr),
            _ => "Exception".to_string(),
        }
    }

    fn is_exception_caught_locally(&self, exc_type: &str) -> bool {
        let short_exc = exc_type.rsplit('.').next().unwrap_or(exc_type);
        for ts in self.try_stack.iter().rev() {
            for h in &ts.handlers {
                if h.rethrows {
                    continue;
                }
                if h.is_bare_except {
                    return true;
                }
                for ct in &h.caught_types {
                    let short_ct = ct.rsplit('.').next().unwrap_or(ct);
                    if short_ct == "BaseException"
                        || short_ct == "Exception"
                        || ct == exc_type
                        || short_ct == short_exc
                        || (short_ct == "OSError"
                            && matches!(
                                short_exc,
                                "IOError" | "FileNotFoundError" | "PermissionError"
                            ))
                        || (short_ct == "ValueError" && short_exc == "JSONDecodeError")
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn is_dict_key_guarded(&self, var_name: &str, key: &str) -> bool {
        if self
            .current_scope()
            .guarded_dict_keys
            .contains(&(var_name.to_string(), key.to_string()))
        {
            return true;
        }
        if self
            .current_scope()
            .var_dict_keys
            .get(var_name)
            .is_some_and(|keys| keys.contains(key))
        {
            return true;
        }
        for g in &self.guard_stack {
            if expr_guards_dict_key(&g.expr, g.in_true_branch, var_name, key) {
                return true;
            }
        }
        false
    }
}

fn classify_guard(test: &Expr, cond_str: &str) -> GuardKind {
    if cond_str == "TYPE_CHECKING" || cond_str.ends_with(".TYPE_CHECKING") {
        return GuardKind::TypeChecking;
    }
    if cond_str.contains("__name__") && cond_str.contains("__main__") {
        return GuardKind::MainGuard;
    }
    if cond_str.contains("sys.platform") || cond_str.contains("os.name") {
        return GuardKind::PlatformGuard;
    }
    if cond_str.contains("sys.version_info") {
        return GuardKind::VersionGuard;
    }
    let _ = test;
    GuardKind::Condition
}

fn extract_caught_types<F: Fn(&Expr) -> String>(
    type_expr: &Option<Box<Expr>>,
    resolve: F,
) -> Vec<String> {
    let Some(expr) = type_expr else {
        return Vec::new();
    };
    match &**expr {
        Expr::Tuple(t) => t.elts.iter().map(&resolve).collect(),
        other => vec![resolve(other)],
    }
}

fn suite_rethrows(body: &[Stmt]) -> bool {
    for stmt in body {
        match stmt {
            Stmt::Raise(_) => return true,
            Stmt::If(if_s) => {
                if suite_rethrows(&if_s.body)
                    || if_s
                        .elif_else_clauses
                        .iter()
                        .any(|c| suite_rethrows(&c.body))
                {
                    return true;
                }
            }
            Stmt::With(w) if suite_rethrows(&w.body) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn suite_handles_cancelled_error(body: &[Stmt], caught_types: &[String]) -> bool {
    if caught_types
        .iter()
        .any(|t| t == "CancelledError" || t.ends_with(".CancelledError"))
    {
        return true;
    }
    for stmt in body {
        if let Stmt::If(if_s) = stmt {
            if suite_rethrows(&if_s.body) {
                return true;
            }
        }
    }
    false
}

fn suite_returns_assumed_ok(body: &[Stmt]) -> bool {
    for stmt in body {
        if let Stmt::Return(ret) = stmt {
            return match &ret.value {
                None => true,
                Some(v) => {
                    is_none_or_false_expr(v) || matches!(&**v, Expr::Dict(_) | Expr::List(_))
                }
            };
        }
    }
    false
}

fn suite_swallows_exception(body: &[Stmt]) -> bool {
    if body.is_empty() {
        return true;
    }
    for stmt in body {
        match stmt {
            Stmt::Pass(_) | Stmt::Continue(_) | Stmt::Break(_) => return true,
            Stmt::Return(_) => return true,
            _ => {}
        }
    }
    !suite_rethrows(body)
}

fn is_none_or_false_expr(expr: &Expr) -> bool {
    match expr {
        Expr::NoneLiteral(_) => true,
        Expr::BooleanLiteral(b) => !b.value,
        _ => false,
    }
}

fn extract_string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

fn extract_string_list(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::List(l) => {
            let mut out = Vec::new();
            for elt in &l.elts {
                out.push(extract_string_literal(elt)?);
            }
            Some(out)
        }
        Expr::Tuple(t) => {
            let mut out = Vec::new();
            for elt in &t.elts {
                out.push(extract_string_literal(elt)?);
            }
            Some(out)
        }
        _ => None,
    }
}

fn extract_dict_literal_keys(expr: &Expr) -> Option<BTreeSet<String>> {
    if let Expr::Dict(d) = expr {
        let mut keys = BTreeSet::new();
        for item in &d.items {
            if let Some(k) = &item.key {
                if let Some(s) = extract_string_literal(k) {
                    keys.insert(s);
                }
            }
        }
        return Some(keys);
    }
    None
}

fn extract_constant_repr(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(format!("{:?}", s.value.to_str())),
        Expr::NumberLiteral(n) => Some(format!("{:?}", n.value)),
        Expr::BooleanLiteral(b) => Some(if b.value { "True" } else { "False" }.to_string()),
        Expr::NoneLiteral(_) => Some("None".to_string()),
        _ => None,
    }
}

fn is_super_call(expr: &Expr) -> bool {
    if let Expr::Call(c) = expr {
        if let Expr::Name(n) = &*c.func {
            return n.id.as_str() == "super";
        }
    }
    false
}

fn is_class_like_callee(callee: &str) -> bool {
    let last = callee.rsplit('.').next().unwrap_or(callee);
    last.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn classify_lock_constructor(callee: &str) -> Option<ResourceKind> {
    if matches!(
        callee,
        "threading.Lock" | "threading.RLock" | "Lock" | "RLock" | "multiprocessing.Lock"
    ) || callee.ends_with(".threading.Lock")
        || callee.ends_with(".threading.RLock")
    {
        return Some(ResourceKind::ThreadingLock);
    }
    if matches!(callee, "asyncio.Lock" | "anyio.Lock") || callee.ends_with(".asyncio.Lock") {
        return Some(ResourceKind::AsyncioLock);
    }
    None
}

fn classify_unowned_resource_alloc(
    callee: &str,
    call: &ruff_python_ast::ExprCall,
) -> Option<ResourceKind> {
    if callee == "open" || callee.ends_with(".open") {
        return Some(ResourceKind::FileHandle);
    }
    if callee == "tempfile.NamedTemporaryFile" || callee == "NamedTemporaryFile" {
        let _ = call;
        return Some(ResourceKind::Tempfile);
    }
    if callee == "subprocess.Popen" || callee == "Popen" {
        return Some(ResourceKind::Subprocess);
    }
    if matches!(
        callee,
        "httpx.Client"
            | "httpx.AsyncClient"
            | "requests.Session"
            | "aiohttp.ClientSession"
            | "Client"
            | "AsyncClient"
            | "Session"
            | "ClientSession"
    ) {
        return Some(ResourceKind::HttpSession);
    }
    None
}

fn classify_known_raising_call(callee: &str, call: &ruff_python_ast::ExprCall) -> Option<String> {
    if callee == "subprocess.check_call" || callee == "subprocess.check_output" {
        return Some("subprocess.CalledProcessError".to_string());
    }
    if callee == "subprocess.run" {
        let has_check_true = call.arguments.keywords.iter().any(|kw| {
            kw.arg.as_ref().map(|a| a.id.as_str()) == Some("check")
                && matches!(&kw.value, Expr::BooleanLiteral(b) if b.value)
        });
        if has_check_true {
            return Some("subprocess.CalledProcessError".to_string());
        }
    }
    if matches!(callee, "json.loads" | "json.load" | "loads") {
        return Some("json.JSONDecodeError".to_string());
    }
    if callee.ends_with(".read_text")
        || callee.ends_with(".read_bytes")
        || callee == "open"
        || callee == "os.remove"
        || callee == "os.unlink"
    {
        return Some("OSError".to_string());
    }
    None
}

fn is_route_or_cli_decorator(dec: &str) -> bool {
    let lower = dec.to_ascii_lowercase();
    lower.ends_with(".get")
        || lower.ends_with(".post")
        || lower.ends_with(".put")
        || lower.ends_with(".delete")
        || lower.ends_with(".patch")
        || lower.ends_with(".route")
        || lower.ends_with(".command")
        || lower.ends_with(".task")
        || lower == "command"
}

pub fn is_magic_dunder_method(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

fn suite_always_exits(body: &[Stmt]) -> bool {
    body.last().is_some_and(|stmt| {
        matches!(
            stmt,
            Stmt::Return(_) | Stmt::Raise(_) | Stmt::Continue(_) | Stmt::Break(_)
        )
    })
}

fn contains_guard_subpattern(expr: &str, pat: &str) -> bool {
    let Some(idx) = expr.find(pat) else {
        return false;
    };
    let before_ok = idx == 0
        || !expr.as_bytes()[idx - 1].is_ascii_alphanumeric() && expr.as_bytes()[idx - 1] != b'_';
    let end = idx + pat.len();
    let after_ok = end >= expr.len()
        || (!expr.as_bytes()[end].is_ascii_alphanumeric()
            && expr.as_bytes()[end] != b'_'
            && expr.as_bytes()[end] != b'.');
    before_ok && after_ok
}

fn expr_guards_dict_key(expr_str: &str, in_true_branch: bool, var_name: &str, key: &str) -> bool {
    let dq_in = format!("\"{key}\" in {var_name}");
    let sq_in = format!("'{key}' in {var_name}");
    let dq_not_in = format!("\"{key}\" not in {var_name}");
    let sq_not_in = format!("'{key}' not in {var_name}");
    let not_dq_in = format!("not \"{key}\" in {var_name}");
    let not_sq_in = format!("not '{key}' in {var_name}");
    let get_dq = format!("{var_name}.get(\"{key}\")");
    let get_sq = format!("{var_name}.get('{key}')");

    if in_true_branch {
        if (contains_guard_subpattern(expr_str, &dq_in)
            && !contains_guard_subpattern(expr_str, &not_dq_in))
            || (contains_guard_subpattern(expr_str, &sq_in)
                && !contains_guard_subpattern(expr_str, &not_sq_in))
            || expr_str.contains(&get_dq)
            || expr_str.contains(&get_sq)
        {
            return true;
        }
    } else if contains_guard_subpattern(expr_str, &dq_not_in)
        || contains_guard_subpattern(expr_str, &sq_not_in)
        || contains_guard_subpattern(expr_str, &not_dq_in)
        || contains_guard_subpattern(expr_str, &not_sq_in)
    {
        return true;
    }
    false
}

fn extract_negated_dict_key_guards(cond_str: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in cond_str.split(" or ") {
        let trimmed = part.trim().trim_matches(|c| c == '(' || c == ')').trim();
        if let Some((lhs, rhs)) = trimmed.split_once(" not in ") {
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            let key = lhs
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| lhs.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
            if let Some(k) = key {
                if !rhs.is_empty() && rhs.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    out.push((rhs.to_string(), k.to_string()));
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("not ") {
            let rest = rest.trim().trim_matches(|c| c == '(' || c == ')').trim();
            if let Some((lhs, rhs)) = rest.split_once(" in ") {
                let lhs = lhs.trim();
                let rhs = rhs.trim();
                let key = lhs
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| lhs.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                if let Some(k) = key {
                    if !rhs.is_empty() && rhs.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        out.push((rhs.to_string(), k.to_string()));
                    }
                }
            }
        }
    }
    out
}

fn is_stub_function_body(body: &[Stmt]) -> bool {
    let mut non_doc = body.iter().enumerate().filter(|(idx, stmt)| {
        if *idx == 0 {
            if let Stmt::Expr(e) = stmt {
                if matches!(&*e.value, Expr::StringLiteral(_)) {
                    return false;
                }
            }
        }
        true
    });
    let Some((_, only_stmt)) = non_doc.next() else {
        return true;
    };
    if non_doc.next().is_some() {
        return false;
    }
    match only_stmt {
        Stmt::Pass(_) => true,
        Stmt::Expr(e) => matches!(&*e.value, Expr::EllipsisLiteral(_)),
        Stmt::Raise(r) => r.exc.as_ref().is_some_and(|exc| match &**exc {
            Expr::Name(n) => n.id.as_str() == "NotImplementedError",
            Expr::Call(c) => {
                matches!(&*c.func, Expr::Name(n) if n.id.as_str() == "NotImplementedError")
            }
            _ => false,
        }),
        _ => false,
    }
}

pub fn normalize_type_str(s: &str) -> String {
    let trimmed = s.trim().replace(' ', "");
    match trimmed.as_str() {
        "typing.Optional[str]" | "Optional[str]" | "str|None" | "None|str" => {
            "str | None".to_string()
        }
        "typing.Optional[int]" | "Optional[int]" | "int|None" | "None|int" => {
            "int | None".to_string()
        }
        "typing.List[str]" | "List[str]" => "list[str]".to_string(),
        "typing.Dict[str,Any]" | "Dict[str,Any]" | "dict[str,Any]" => "dict[str, Any]".to_string(),
        _ => s.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// AST / CFG Skeleton Canonicalizer for `semantic-clone`
// ---------------------------------------------------------------------------

struct SkeletonCanonicalizer {
    var_map: BTreeMap<String, String>,
    next_var_id: u32,
    tokens: Vec<String>,
    stmt_count: u32,
}

impl SkeletonCanonicalizer {
    fn new(func: &StmtFunctionDef) -> Self {
        let mut var_map = BTreeMap::new();
        let mut p_idx = 0u32;
        for p in func.parameters.iter_non_variadic_params() {
            let name = p.parameter.name.id.to_string();
            if name != "self" && name != "cls" {
                var_map.insert(name, format!("$p{p_idx}"));
                p_idx += 1;
            }
        }
        if let Some(vararg) = &func.parameters.vararg {
            var_map.insert(vararg.name.id.to_string(), format!("$p{p_idx}"));
            p_idx += 1;
        }
        if let Some(kwarg) = &func.parameters.kwarg {
            var_map.insert(kwarg.name.id.to_string(), format!("$p{p_idx}"));
        }
        Self {
            var_map,
            next_var_id: 0,
            tokens: Vec::new(),
            stmt_count: 0,
        }
    }

    fn bind_local(&mut self, name: &str) -> String {
        if let Some(existing) = self.var_map.get(name) {
            return existing.clone();
        }
        let id = format!("$v{}", self.next_var_id);
        self.next_var_id += 1;
        self.var_map.insert(name.to_string(), id.clone());
        id
    }

    fn resolve_name(&self, name: &str) -> String {
        if let Some(mapped) = self.var_map.get(name) {
            return mapped.clone();
        }
        name.to_string()
    }

    fn bind_target(&mut self, target: &Expr) {
        match target {
            Expr::Name(n) => {
                let id = self.bind_local(n.id.as_str());
                self.tokens.push(format!("T({id})"));
            }
            Expr::Tuple(t) => {
                self.tokens.push("TUP(".to_string());
                for e in &t.elts {
                    self.bind_target(e);
                }
                self.tokens.push(")".to_string());
            }
            Expr::List(l) => {
                self.tokens.push("LST(".to_string());
                for e in &l.elts {
                    self.bind_target(e);
                }
                self.tokens.push(")".to_string());
            }
            other => self.visit_expr(other),
        }
    }

    fn visit_suite(&mut self, body: &[Stmt], skip_leading_docstring: bool) {
        for (idx, stmt) in body.iter().enumerate() {
            if skip_leading_docstring && idx == 0 {
                if let Stmt::Expr(e) = stmt {
                    if matches!(&*e.value, Expr::StringLiteral(_)) {
                        continue;
                    }
                }
            }
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        self.stmt_count += 1;
        match stmt {
            Stmt::Return(r) => {
                self.tokens.push("RET(".to_string());
                if let Some(v) = &r.value {
                    self.visit_expr(v);
                }
                self.tokens.push(")".to_string());
            }
            Stmt::Assign(a) => {
                self.tokens.push("ASN(".to_string());
                for t in &a.targets {
                    self.bind_target(t);
                }
                self.visit_expr(&a.value);
                self.tokens.push(")".to_string());
            }
            Stmt::AnnAssign(a) => {
                self.tokens.push("ASN(".to_string());
                self.bind_target(&a.target);
                if let Some(v) = &a.value {
                    self.visit_expr(v);
                }
                self.tokens.push(")".to_string());
            }
            Stmt::AugAssign(a) => {
                self.tokens.push(format!("AUG({:?},", a.op));
                self.visit_expr(&a.target);
                self.visit_expr(&a.value);
                self.tokens.push(")".to_string());
            }
            Stmt::If(if_s) => {
                self.tokens.push("IF(".to_string());
                self.visit_expr(&if_s.test);
                self.tokens.push("{".to_string());
                self.visit_suite(&if_s.body, false);
                self.tokens.push("}".to_string());
                for clause in &if_s.elif_else_clauses {
                    if let Some(t) = &clause.test {
                        self.tokens.push("ELIF(".to_string());
                        self.visit_expr(t);
                        self.tokens.push("{".to_string());
                        self.visit_suite(&clause.body, false);
                        self.tokens.push("})".to_string());
                    } else {
                        self.tokens.push("ELSE{".to_string());
                        self.visit_suite(&clause.body, false);
                        self.tokens.push("}".to_string());
                    }
                }
                self.tokens.push(")".to_string());
            }
            Stmt::For(f) => {
                self.tokens.push(if f.is_async {
                    "AFOR(".to_string()
                } else {
                    "FOR(".to_string()
                });
                self.bind_target(&f.target);
                self.visit_expr(&f.iter);
                self.tokens.push("{".to_string());
                self.visit_suite(&f.body, false);
                self.tokens.push("})".to_string());
            }
            Stmt::While(w) => {
                self.tokens.push("WHILE(".to_string());
                self.visit_expr(&w.test);
                self.tokens.push("{".to_string());
                self.visit_suite(&w.body, false);
                self.tokens.push("})".to_string());
            }
            Stmt::With(w) => {
                self.tokens.push(if w.is_async {
                    "AWITH(".to_string()
                } else {
                    "WITH(".to_string()
                });
                for item in &w.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(v) = &item.optional_vars {
                        self.bind_target(v);
                    }
                }
                self.tokens.push("{".to_string());
                self.visit_suite(&w.body, false);
                self.tokens.push("})".to_string());
            }
            Stmt::Try(t) => {
                self.tokens.push("TRY{".to_string());
                self.visit_suite(&t.body, false);
                self.tokens.push("}".to_string());
                for h in &t.handlers {
                    let ExceptHandler::ExceptHandler(eh) = h;
                    self.tokens.push("CATCH(".to_string());
                    if let Some(ty) = &eh.type_ {
                        self.visit_expr(ty);
                    }
                    if let Some(name) = &eh.name {
                        let _ = self.bind_local(name.id.as_str());
                    }
                    self.tokens.push("{".to_string());
                    self.visit_suite(&eh.body, false);
                    self.tokens.push("})".to_string());
                }
                if !t.finalbody.is_empty() {
                    self.tokens.push("FINALLY{".to_string());
                    self.visit_suite(&t.finalbody, false);
                    self.tokens.push("}".to_string());
                }
            }
            Stmt::Raise(r) => {
                self.tokens.push("RAISE(".to_string());
                if let Some(e) = &r.exc {
                    self.visit_expr(e);
                }
                self.tokens.push(")".to_string());
            }
            Stmt::Assert(a) => {
                self.tokens.push("ASSERT(".to_string());
                self.visit_expr(&a.test);
                self.tokens.push(")".to_string());
            }
            Stmt::Expr(e) => {
                self.tokens.push("EXPR(".to_string());
                self.visit_expr(&e.value);
                self.tokens.push(")".to_string());
            }
            Stmt::Pass(_) => self.tokens.push("PASS".to_string()),
            Stmt::Break(_) => self.tokens.push("BREAK".to_string()),
            Stmt::Continue(_) => self.tokens.push("CONT".to_string()),
            _ => {}
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Name(n) => {
                self.tokens.push(self.resolve_name(n.id.as_str()));
            }
            Expr::Attribute(a) => {
                self.visit_expr(&a.value);
                self.tokens.push(format!(".{}", a.attr.id));
            }
            Expr::Call(c) => {
                self.tokens.push("CALL(".to_string());
                self.visit_expr(&c.func);
                self.tokens.push("(".to_string());
                for arg in &c.arguments.args {
                    self.visit_expr(arg);
                    self.tokens.push(",".to_string());
                }
                for kw in &c.arguments.keywords {
                    if let Some(k) = &kw.arg {
                        self.tokens.push(format!("{}=", k.id));
                    }
                    self.visit_expr(&kw.value);
                    self.tokens.push(",".to_string());
                }
                self.tokens.push("))".to_string());
            }
            Expr::BinOp(b) => {
                self.tokens.push(format!("BIN({:?},", op_name(b.op)));
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
                self.tokens.push(")".to_string());
            }
            Expr::UnaryOp(u) => {
                self.tokens.push(format!("UN({:?},", unary_name(u.op)));
                self.visit_expr(&u.operand);
                self.tokens.push(")".to_string());
            }
            Expr::BoolOp(b) => {
                self.tokens.push(format!("BOOL({:?},", b.op));
                for v in &b.values {
                    self.visit_expr(v);
                }
                self.tokens.push(")".to_string());
            }
            Expr::Compare(c) => {
                let ops: Vec<&str> = c.ops.iter().map(|o| cmp_name(*o)).collect();
                self.tokens.push(format!("CMP({},", ops.join(",")));
                for opnd in &c.operands {
                    self.visit_expr(opnd);
                }
                self.tokens.push(")".to_string());
            }
            Expr::Subscript(s) => {
                self.tokens.push("SUB(".to_string());
                self.visit_expr(&s.value);
                self.visit_expr(&s.slice);
                self.tokens.push(")".to_string());
            }
            Expr::Await(a) => {
                self.tokens.push("AWAIT(".to_string());
                self.visit_expr(&a.value);
                self.tokens.push(")".to_string());
            }
            Expr::Dict(d) => {
                self.tokens.push(format!("DICT({})", d.items.len()));
                for item in &d.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&item.value);
                }
            }
            Expr::List(l) => {
                self.tokens.push(format!("LIST({})", l.elts.len()));
                for e in &l.elts {
                    self.visit_expr(e);
                }
            }
            Expr::Tuple(t) => {
                self.tokens.push(format!("TUPLE({})", t.elts.len()));
                for e in &t.elts {
                    self.visit_expr(e);
                }
            }
            Expr::StringLiteral(_) | Expr::FString(_) => {
                self.tokens.push("$STR".to_string());
            }
            Expr::NumberLiteral(_) => {
                self.tokens.push("$NUM".to_string());
            }
            Expr::BooleanLiteral(b) => {
                self.tokens
                    .push(if b.value { "TRUE" } else { "FALSE" }.to_string());
            }
            Expr::NoneLiteral(_) => {
                self.tokens.push("NONE".to_string());
            }
            _ => {
                self.tokens.push("EXPR".to_string());
            }
        }
    }
}

fn op_name(op: Operator) -> &'static str {
    match op {
        Operator::Add => "+",
        Operator::Sub => "-",
        Operator::Mult => "*",
        Operator::MatMult => "@",
        Operator::Div => "/",
        Operator::Mod => "%",
        Operator::Pow => "**",
        Operator::LShift => "<<",
        Operator::RShift => ">>",
        Operator::BitOr => "|",
        Operator::BitXor => "^",
        Operator::BitAnd => "&",
        Operator::FloorDiv => "//",
    }
}

fn unary_name(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Invert => "~",
        UnaryOp::Not => "not",
        UnaryOp::UAdd => "+",
        UnaryOp::USub => "-",
    }
}

fn cmp_name(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::NotEq => "!=",
        CmpOp::Lt => "<",
        CmpOp::LtE => "<=",
        CmpOp::Gt => ">",
        CmpOp::GtE => ">=",
        CmpOp::Is => "is",
        CmpOp::IsNot => "is_not",
        CmpOp::In => "in",
        CmpOp::NotIn => "not_in",
    }
}

pub fn compute_function_skeleton(func: &StmtFunctionDef) -> (u64, Vec<u64>, String, u32) {
    let mut canon = SkeletonCanonicalizer::new(func);
    canon.visit_suite(&func.body, true);
    let skeleton = canon.tokens.join(" ");
    if canon.stmt_count == 0
        || skeleton == "PASS"
        || skeleton == "EXPR( EXPR )"
        || skeleton.starts_with("RAISE( CALL( NotImplementedError")
        || skeleton == "RAISE( NotImplementedError )"
    {
        return (0, Vec::new(), skeleton, canon.stmt_count);
    }
    let hash = xxh3_64(skeleton.as_bytes());
    let mut features = BTreeSet::new();
    // 3-gram token shingles for near-duplicate similarity
    if canon.tokens.len() >= 3 {
        for win in canon.tokens.windows(3) {
            let joined = format!("{}|{}|{}", win[0], win[1], win[2]);
            features.insert(xxh3_64(joined.as_bytes()));
        }
    } else {
        features.insert(hash);
    }
    (
        hash,
        features.into_iter().collect(),
        skeleton,
        canon.stmt_count,
    )
}
