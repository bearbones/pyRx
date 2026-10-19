# Copyright (c) 2026 The pyrex Authors
# Original author: Alex Mason
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Buck2 rules and macros for pyrex (`megascope` call-graph indexing and `check` cross-module analysis).

Architecture: 3-Tier Map-Reduce Call-Graph Indexing in Buck2
============================================================
1. **Per-Module (`.pyrx` per `.py` file)**:
   `pyrex_module_index` indexes a single `.py` file in its own cacheable, parallel
   Buck2 action (`pyrex megascope index --threads 1 --source <src.py> --index <mod.pyrx>`).

2. **Per-Target (`.pyrx` per `python_library` / `python_binary` / `python_test`)**:
   `pyrex_target_index` merges a target's per-module `.pyrx` shards into `<target>.pyrx`
   (`pyrex megascope merge --output <target.pyrx> --input-list <list>`) and propagates a
   `PyrexIndexTSet` (`transitive_set`) across `deps`.
   Sub-targets provide direct access to every granularity:
   - `buck2 build //pkg:lib_pyrex`               -> per-target `lib_pyrex.pyrx`
   - `buck2 build //pkg:lib_pyrex[target]`       -> per-target `lib_pyrex.pyrx`
   - `buck2 build //pkg:lib_pyrex[module]`       -> all per-module `.pyrx` shards of `lib`
   - `buck2 build //pkg:lib_pyrex[module/a.py]`  -> single per-module `.pyrx` shard for `a.py`
   - `buck2 build //pkg:lib_pyrex[global]`       -> transitive global `.pyrx` across `lib` + `deps`

3. **Global Index (`.pyrx` across target graphs)**:
   `pyrex_global_index` reduces `PyrexIndexTSet` across any set of root targets into a
   single global `.pyrx` call-graph, import-graph, and control-flow index with
   diamond-dependency deduplication.
"""

# ---------------------------------------------------------------------------
# Providers & Transitive Sets
# ---------------------------------------------------------------------------

def _project_index_path(artifact: Artifact):
    return cmd_args(artifact)

PyrexIndexTSet = transitive_set(
    args_projections = {
        "inputs": _project_index_path,
    },
)

PyrexIndexInfo = provider(
    doc = "Carries per-module, per-target, and transitive .pyrx call-graph artifacts.",
    fields = [
        "global_index",
        "module_shards",
        "module_tset",
        "target_index",
        "target_tset",
    ],
)

_DepSets = record(
    target_children = list,
    module_children = list,
)

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _pyrex_exe(ctx: AnalysisContext) -> cmd_args:
    if getattr(ctx.attrs, "pyrex_bin", None) != None:
        return cmd_args(ctx.attrs.pyrex_bin[RunInfo])
    exe = getattr(ctx.attrs, "pyrex_executable", "pyrex")
    return cmd_args(exe)

def _collect_dep_sets(deps: list) -> _DepSets:
    target_children = []
    module_children = []
    for dep in deps:
        if PyrexIndexInfo in dep:
            info = dep[PyrexIndexInfo]
            if info.target_tset != None:
                target_children.append(info.target_tset)
            if info.module_tset != None:
                module_children.append(info.module_tset)
    return _DepSets(
        target_children = target_children,
        module_children = module_children,
    )

def _append_bake_options(cmd: cmd_args, ctx: AnalysisContext):
    if getattr(ctx.attrs, "project_root", "") != "":
        cmd.add("--project-root", ctx.attrs.project_root)
    if getattr(ctx.attrs, "module_name", "") != "":
        cmd.add("--module-name", ctx.attrs.module_name)
    for pat in getattr(ctx.attrs, "collapse_paths", []):
        cmd.add("--collapse-paths", pat)
    for ep in getattr(ctx.attrs, "entry_points", []):
        cmd.add("--entry-point", ep)

def _declare_module_bake_action(
        ctx: AnalysisContext,
        src: Artifact,
        out_name: str,
        module_name: str = "") -> Artifact:
    mod_out = ctx.actions.declare_output(out_name)
    cmd = cmd_args(
        _pyrex_exe(ctx),
        "megascope",
        "index",
        "--threads",
        "1",
        "--no-wait",
        "--force",
        "--source",
        src,
        "--index",
        mod_out.as_output(),
    )
    if module_name != "":
        cmd.add("--module-name", module_name)
    _append_bake_options(cmd, ctx)
    ctx.actions.run(
        cmd,
        category = "pyrex_module_index",
        identifier = src.short_path,
    )
    return mod_out

def _declare_merge_action(
        ctx: AnalysisContext,
        out_name: str,
        inputs_args: cmd_args,
        identifier: str) -> Artifact:
    merged_out = ctx.actions.declare_output(out_name)
    input_list = ctx.actions.write(out_name + ".inputs.txt", inputs_args)
    cmd = cmd_args(
        _pyrex_exe(ctx),
        "megascope",
        "merge",
        "--no-wait",
        "--allow-mismatch",
        "--output",
        merged_out.as_output(),
        "--input-list",
        input_list,
    )
    cmd.add(cmd_args(hidden = [inputs_args]))
    for ep in getattr(ctx.attrs, "entry_points", []):
        cmd.add("--entry-point", ep)
    ctx.actions.run(
        cmd,
        category = "pyrex_index_merge",
        identifier = identifier,
    )
    return merged_out

# ---------------------------------------------------------------------------
# Rule 1: `pyrex_module_index` — Single-Module `.pyrx` Shard
# ---------------------------------------------------------------------------

def _pyrex_module_index_impl(ctx: AnalysisContext) -> list[Provider]:
    dep_sets = _collect_dep_sets(ctx.attrs.deps)
    out_name = ctx.attrs.out if ctx.attrs.out else (ctx.label.name + ".pyrx")
    mod_out = _declare_module_bake_action(
        ctx = ctx,
        src = ctx.attrs.src,
        out_name = out_name,
        module_name = ctx.attrs.module_name,
    )

    mod_tset = ctx.actions.tset(
        PyrexIndexTSet,
        value = mod_out,
        children = dep_sets.module_children,
    )
    target_tset = ctx.actions.tset(
        PyrexIndexTSet,
        value = mod_out,
        children = dep_sets.target_children,
    )

    return [
        DefaultInfo(default_output = mod_out),
        PyrexIndexInfo(
            global_index = mod_out,
            module_shards = {ctx.attrs.src.short_path: mod_out},
            module_tset = mod_tset,
            target_index = mod_out,
            target_tset = target_tset,
        ),
    ]

pyrex_module_index = rule(
    impl = _pyrex_module_index_impl,
    doc = "Indexes a single Python module (.py) into a hermetic .pyrx call-graph and control-flow shard.",
    attrs = {
        "collapse_paths": attrs.list(attrs.string(), default = []),
        "deps": attrs.list(attrs.dep(providers = [PyrexIndexInfo]), default = []),
        "entry_points": attrs.list(attrs.string(), default = []),
        "module_name": attrs.string(default = ""),
        "out": attrs.option(attrs.string(), default = None),
        "project_root": attrs.string(default = ""),
        "pyrex_bin": attrs.option(attrs.exec_dep(providers = [RunInfo]), default = None),
        "pyrex_executable": attrs.string(default = "pyrex"),
        "src": attrs.source(),
    },
)

# ---------------------------------------------------------------------------
# Rule 2: `pyrex_target_index` — Per-Target `.pyrx` Index + Transitive Propagation
# ---------------------------------------------------------------------------

def _pyrex_target_index_impl(ctx: AnalysisContext) -> list[Provider]:
    dep_sets = _collect_dep_sets(ctx.attrs.deps)
    mod_shards = {}
    mod_list = []

    for i, src in enumerate(ctx.attrs.srcs):
        shard_name = "module/{}_{}.pyrx".format(i, src.short_path.replace("/", "_"))
        shard = _declare_module_bake_action(
            ctx = ctx,
            src = src,
            out_name = shard_name,
        )
        mod_shards[src.short_path] = shard
        mod_list.append(shard)

    own_mod_tsets = [ctx.actions.tset(PyrexIndexTSet, value = s) for s in mod_list]
    all_mod_children = own_mod_tsets + dep_sets.module_children
    mod_tset = ctx.actions.tset(PyrexIndexTSet, children = all_mod_children) if all_mod_children else None

    target_out_name = ctx.attrs.out if ctx.attrs.out else (ctx.label.name + ".pyrx")
    if len(mod_list) == 1:
        target_out = mod_list[0]
    elif len(mod_list) > 1:
        target_out = _declare_merge_action(
            ctx = ctx,
            out_name = target_out_name,
            inputs_args = cmd_args(mod_list),
            identifier = ctx.label.name + "_target",
        )
    elif dep_sets.target_children:
        dep_only_tset = ctx.actions.tset(PyrexIndexTSet, children = dep_sets.target_children)
        target_out = _declare_merge_action(
            ctx = ctx,
            out_name = target_out_name,
            inputs_args = dep_only_tset.project_as_args("inputs"),
            identifier = ctx.label.name + "_deps_only",
        )
    else:
        fail("pyrex_target_index '{}' requires at least one entry in `srcs` or `deps`".format(ctx.label.name))

    if len(mod_list) > 0:
        target_tset = ctx.actions.tset(
            PyrexIndexTSet,
            value = target_out,
            children = dep_sets.target_children,
        )
    elif dep_sets.target_children:
        target_tset = ctx.actions.tset(
            PyrexIndexTSet,
            children = dep_sets.target_children,
        )
    else:
        target_tset = ctx.actions.tset(PyrexIndexTSet, value = target_out)

    if dep_sets.target_children and len(mod_list) > 0:
        global_out = _declare_merge_action(
            ctx = ctx,
            out_name = ctx.label.name + ".global.pyrx",
            inputs_args = target_tset.project_as_args("inputs"),
            identifier = ctx.label.name + "_global",
        )
    else:
        global_out = target_out

    mod_sub_targets = {
        path: [DefaultInfo(default_output = art)]
        for path, art in mod_shards.items()
    }
    sub_targets = {
        "global": [DefaultInfo(default_output = global_out)],
        "module": [DefaultInfo(default_outputs = mod_list, sub_targets = mod_sub_targets)],
        "target": [DefaultInfo(default_output = target_out)],
    }

    return [
        DefaultInfo(
            default_output = target_out,
            sub_targets = sub_targets,
        ),
        PyrexIndexInfo(
            global_index = global_out,
            module_shards = mod_shards,
            module_tset = mod_tset,
            target_index = target_out,
            target_tset = target_tset,
        ),
    ]

pyrex_target_index = rule(
    impl = _pyrex_target_index_impl,
    doc = "Indexes a Python library or binary target by baking each module in parallel and merging into <target>.pyrx.",
    attrs = {
        "collapse_paths": attrs.list(attrs.string(), default = []),
        "deps": attrs.list(attrs.dep(providers = [PyrexIndexInfo]), default = []),
        "entry_points": attrs.list(attrs.string(), default = []),
        "out": attrs.option(attrs.string(), default = None),
        "project_root": attrs.string(default = ""),
        "pyrex_bin": attrs.option(attrs.exec_dep(providers = [RunInfo]), default = None),
        "pyrex_executable": attrs.string(default = "pyrex"),
        "srcs": attrs.list(attrs.source(), default = []),
    },
)

# ---------------------------------------------------------------------------
# Rule 3: `pyrex_global_index` — Whole-Graph `.pyrx` Reducer
# ---------------------------------------------------------------------------

def _pyrex_global_index_impl(ctx: AnalysisContext) -> list[Provider]:
    dep_sets = _collect_dep_sets(ctx.attrs.deps)
    children = dep_sets.module_children if ctx.attrs.granularity == "module" else dep_sets.target_children
    if not children:
        fail("pyrex_global_index '{}' requires at least one dependency providing PyrexIndexInfo".format(ctx.label.name))

    merged_tset = ctx.actions.tset(PyrexIndexTSet, children = children)
    out_name = ctx.attrs.out if ctx.attrs.out else (ctx.label.name + ".pyrx")
    global_out = _declare_merge_action(
        ctx = ctx,
        out_name = out_name,
        inputs_args = merged_tset.project_as_args("inputs"),
        identifier = ctx.label.name,
    )

    return [
        DefaultInfo(default_output = global_out),
        PyrexIndexInfo(
            global_index = global_out,
            module_shards = {},
            module_tset = ctx.actions.tset(PyrexIndexTSet, children = dep_sets.module_children) if dep_sets.module_children else None,
            target_index = global_out,
            target_tset = merged_tset,
        ),
    ]

pyrex_global_index = rule(
    impl = _pyrex_global_index_impl,
    doc = "Reduces transitive .pyrx target or module shards across a dependency graph into a single global .pyrx index.",
    attrs = {
        "deps": attrs.list(attrs.dep(providers = [PyrexIndexInfo])),
        "entry_points": attrs.list(attrs.string(), default = []),
        "granularity": attrs.enum(["target", "module"], default = "target"),
        "out": attrs.option(attrs.string(), default = None),
        "pyrex_bin": attrs.option(attrs.exec_dep(providers = [RunInfo]), default = None),
        "pyrex_executable": attrs.string(default = "pyrex"),
    },
)

# ---------------------------------------------------------------------------
# Rule 4: `pyrex_query` — Run a `megascope` Query Verb Against an Index
# ---------------------------------------------------------------------------

def _pyrex_query_impl(ctx: AnalysisContext) -> list[Provider]:
    info = ctx.attrs.index[PyrexIndexInfo]
    idx_artifact = info.global_index if ctx.attrs.use_global else info.target_index
    ext = ctx.attrs.format
    out_name = ctx.attrs.out if ctx.attrs.out else "{}.{}.{}".format(ctx.label.name, ctx.attrs.tool, ext)
    out = ctx.actions.declare_output(out_name)

    query_cmd = cmd_args(
        _pyrex_exe(ctx),
        "megascope",
        ctx.attrs.tool,
        "--index",
        idx_artifact,
        "--format",
        ctx.attrs.format,
    )
    if ctx.attrs.pretty and ctx.attrs.format == "json":
        query_cmd.add("--pretty")
    if ctx.attrs.args_json != "":
        query_cmd.add("--args", ctx.attrs.args_json)
    for flag in ctx.attrs.flags:
        query_cmd.add(flag)

    script = cmd_args(
        "/bin/sh",
        "-c",
        'OUT="$1"; shift; "$@" > "$OUT"; rc=$?; if [ $rc -eq 0 ] || [ $rc -eq 1 ]; then exit 0; else exit $rc; fi',
        "--",
        out.as_output(),
        query_cmd,
    )
    ctx.actions.run(
        script,
        category = "pyrex_query",
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = out)]

pyrex_query = rule(
    impl = _pyrex_query_impl,
    doc = "Runs a pyrex megascope query tool against a .pyrx index target and captures the output.",
    attrs = {
        "args_json": attrs.string(default = ""),
        "flags": attrs.list(attrs.string(), default = []),
        "format": attrs.enum(["json", "ndjson", "tsv"], default = "json"),
        "index": attrs.dep(providers = [PyrexIndexInfo]),
        "out": attrs.option(attrs.string(), default = None),
        "pretty": attrs.bool(default = True),
        "pyrex_bin": attrs.option(attrs.exec_dep(providers = [RunInfo]), default = None),
        "pyrex_executable": attrs.string(default = "pyrex"),
        "tool": attrs.string(),
        "use_global": attrs.bool(default = True),
    },
)

# ---------------------------------------------------------------------------
# Rule 5: `pyrex_check` — Cross-Module Static Analysis Gate
# ---------------------------------------------------------------------------

def _pyrex_check_impl(ctx: AnalysisContext) -> list[Provider]:
    info = ctx.attrs.index[PyrexIndexInfo]
    idx_artifact = info.global_index if ctx.attrs.use_global else info.target_index
    ext = "sarif" if ctx.attrs.format == "sarif" else ("json" if ctx.attrs.format == "json" else "txt")
    out_name = ctx.attrs.out if ctx.attrs.out else "{}.check.{}".format(ctx.label.name, ext)
    out = ctx.actions.declare_output(out_name)

    cmd = cmd_args(
        _pyrex_exe(ctx),
        "check",
        "--index",
        idx_artifact,
        "--format",
        ctx.attrs.format,
        "--fail-on",
        ctx.attrs.fail_on,
        "--output",
        out.as_output(),
    )
    if ctx.attrs.checks != "":
        cmd.add("--checks", ctx.attrs.checks)
    if ctx.attrs.checks_config != None:
        cmd.add("--checks-config", ctx.attrs.checks_config)
    if ctx.attrs.baseline != None:
        cmd.add("--baseline", ctx.attrs.baseline)

    ctx.actions.run(
        cmd,
        category = "pyrex_check",
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = out)]

pyrex_check = rule(
    impl = _pyrex_check_impl,
    doc = "Runs pyrex check across a .pyrx index target, producing text, JSON, or SARIF 2.1.0 diagnostics.",
    attrs = {
        "baseline": attrs.option(attrs.source(), default = None),
        "checks": attrs.string(default = ""),
        "checks_config": attrs.option(attrs.source(), default = None),
        "fail_on": attrs.enum(["note", "warning", "error", "none"], default = "none"),
        "format": attrs.enum(["text", "json", "sarif"], default = "sarif"),
        "index": attrs.dep(providers = [PyrexIndexInfo]),
        "out": attrs.option(attrs.string(), default = None),
        "pyrex_bin": attrs.option(attrs.exec_dep(providers = [RunInfo]), default = None),
        "pyrex_executable": attrs.string(default = "pyrex"),
        "use_global": attrs.bool(default = True),
    },
)

# ---------------------------------------------------------------------------
# Drop-In Wrapper Macros: `pyrex_python_library`, `pyrex_python_binary`
# ---------------------------------------------------------------------------

def _to_pyrex_dep(dep: str, suffix: str) -> str:
    if ":" in dep:
        return dep + suffix
    pkg_leaf = dep.rsplit("/", 1)[-1]
    return dep + ":" + pkg_leaf + suffix

def pyrex_python_library(
        name: str,
        srcs: list = [],
        deps: list = [],
        pyrex_deps = None,
        pyrex_suffix: str = "_pyrex",
        pyrex_bin = None,
        pyrex_executable: str = "pyrex",
        pyrex_collapse_paths: list = [],
        pyrex_entry_points: list = [],
        visibility: list = ["PUBLIC"],
        **kwargs):
    """Wraps `prelude.python_library` and automatically defines a companion `<name>_pyrex` (`pyrex_target_index`)."""
    if hasattr(native, "python_library"):
        native.python_library(
            name = name,
            srcs = srcs,
            deps = deps,
            visibility = visibility,
            **kwargs
        )
    idx_deps = pyrex_deps if pyrex_deps != None else [_to_pyrex_dep(d, pyrex_suffix) for d in deps]
    pyrex_target_index(
        name = name + pyrex_suffix,
        srcs = srcs,
        deps = idx_deps,
        collapse_paths = pyrex_collapse_paths,
        entry_points = pyrex_entry_points,
        pyrex_bin = pyrex_bin,
        pyrex_executable = pyrex_executable,
        visibility = visibility,
    )

def pyrex_python_binary(
        name: str,
        srcs: list = [],
        main_module: str = "",
        deps: list = [],
        pyrex_deps = None,
        pyrex_suffix: str = "_pyrex",
        pyrex_bin = None,
        pyrex_executable: str = "pyrex",
        pyrex_collapse_paths: list = [],
        pyrex_entry_points: list = [],
        visibility: list = ["PUBLIC"],
        **kwargs):
    """Wraps `prelude.python_binary` and automatically defines a companion `<name>_pyrex` (`pyrex_target_index`)."""
    if hasattr(native, "python_binary"):
        native.python_binary(
            name = name,
            srcs = srcs,
            main_module = main_module,
            deps = deps,
            visibility = visibility,
            **kwargs
        )
    idx_deps = pyrex_deps if pyrex_deps != None else [_to_pyrex_dep(d, pyrex_suffix) for d in deps]
    eps = list(pyrex_entry_points)
    if main_module != "":
        eps.append(main_module + ".<module>")
        eps.append(main_module + ".main")
    pyrex_target_index(
        name = name + pyrex_suffix,
        srcs = srcs,
        deps = idx_deps,
        collapse_paths = pyrex_collapse_paths,
        entry_points = eps,
        pyrex_bin = pyrex_bin,
        pyrex_executable = pyrex_executable,
        visibility = visibility,
    )
