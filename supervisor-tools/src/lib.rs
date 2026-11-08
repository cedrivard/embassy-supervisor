#![deny(missing_docs)]

//! Host-side tooling library for [`embassy-supervisor`](https://docs.rs/embassy-supervisor).
//!
//! This crate powers the `supervisor-mermaid` and `supervisor-lint` binaries.
//! It reads `supervisor_graph!` / `supervisor_fragment!` / `compose_graph!`
//! declarations straight from Rust source, scans `#[dataflow]` fn bodies, and
//! renders or analyses the resulting task graph.
//!
//! Most users interact with the crate through its binaries rather than this
//! library API.

pub mod find;
/// Collecting input files from directories, Cargo manifests, and dependency sources.
pub mod inputs;
/// Linting: orphan reads, dead writes and public gates.
pub mod lint;
pub mod model;
/// Output helpers: mermaid.live links, HTML pages, and markdown updates.
pub mod out;
/// Rendering task graphs as Mermaid diagrams.
pub mod render;
/// Rendering node lifecycle state diagrams.
pub mod states;

use embassy_supervisor_syntax::{
    Item, TaskSource, apply_default_executor, normalize_fragment_crate, substitute_dollar_crate,
};
use proc_macro2::TokenStream;
use quote::quote;
use std::str::FromStr;

pub use embassy_supervisor_syntax::Access;
pub use find::{Decl, DeclKind, Error, parse_source};
pub use lint::{LintCats, dataflow_lints, gate_lints};
pub use model::{FullModel, TaskKind, full_model};
pub use render::{Options, legend_diagram};

/// A dataflow access discovered in a scanned source file.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// The module path of the fn holding the access: the file's (see
    /// [`scan_source`]), then the inline `mod`s around the fn.
    pub module: Vec<String>,
    /// `true` when the file's path was resolved under a `src/` component;
    /// `false` for the stem fallback (`t.rs`, `-`).
    pub rooted: bool,
    /// The directory above `src/`, `-` mapped to `_`, when the file's path has
    /// one (`common/src/x.rs` gives `common`).
    pub crate_dir: Option<String>,
    /// The access details: function, verb, signal path, and direction.
    pub access: Access,
    /// `true` if the path was relative (`self::`, `super::`, or bare).
    pub relative: bool,
}

/// Scan a parsed source file for `#[dataflow]` accesses and append them to `out`.
///
/// `origin` is the file path. Each access gets a module path: the file's
/// place after its last `src` directory (`src/imu/driver.rs` is
/// `imu::driver`; `main.rs`, `lib.rs` and the `src/bin/` roots are the crate
/// root; a `mod.rs` is its directory) or, outside a `src/` tree, the file's
/// stem, then the inline `mod`s around the fn. It qualifies relative signal
/// paths and binds the fn to a declaration.
pub fn scan_source(file: &syn::File, origin: &str, out: &mut Vec<Discovered>) {
    let start = out.len();
    let FileModule {
        mut path,
        rooted,
        crate_dir,
    } = file_module(origin);
    walk_dataflow(&file.items, &mut path, rooted, crate_dir.as_ref(), out);
    if out.len() == start {
        return;
    }
    let aliases = use_aliases(file);
    for d in &mut out[start..] {
        (d.access.path, d.relative) = resolve_path(&d.access.path, &aliases);
    }
}

/// Every `#[dataflow]` fn's accesses, inline modules and `impl` blocks
/// included, in the order `embassy_supervisor_syntax::scan_dataflow` visits
/// them. Paths come back as written: [`scan_source`] resolves them and sets
/// `relative`.
fn walk_dataflow(
    items: &[syn::Item],
    module: &mut Vec<String>,
    rooted: bool,
    crate_dir: Option<&String>,
    out: &mut Vec<Discovered>,
) {
    for item in items {
        match item {
            syn::Item::Fn(f) => {
                dataflow_fn(&f.attrs, &f.sig, &f.block, module, rooted, crate_dir, out)
            }
            syn::Item::Impl(im) => {
                for ii in &im.items {
                    if let syn::ImplItem::Fn(f) = ii {
                        dataflow_fn(&f.attrs, &f.sig, &f.block, module, rooted, crate_dir, out);
                    }
                }
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    module.push(m.ident.to_string());
                    walk_dataflow(inner, module, rooted, crate_dir, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}

/// One fn's accesses, from the syntax crate's walker, each tagged with the
/// fn's module.
fn dataflow_fn(
    attrs: &[syn::Attribute],
    sig: &syn::Signature,
    block: &syn::Block,
    module: &[String],
    rooted: bool,
    crate_dir: Option<&String>,
    out: &mut Vec<Discovered>,
) {
    let mut accesses = Vec::new();
    embassy_supervisor_syntax::scan_fn_accesses(attrs, sig, block, &mut accesses);
    out.extend(accesses.into_iter().map(|access| Discovered {
        module: module.to_vec(),
        rooted,
        crate_dir: crate_dir.cloned(),
        access,
        relative: false,
    }));
}

/// A `static` whose type is one of the supervisor's gates (`Backed`, `Leased`,
/// `VetoGate`), found in a scanned source file.
#[derive(Debug, Clone)]
pub struct GateStatic {
    /// The source file.
    pub file: String,
    /// The line the static is declared on.
    pub line: usize,
    /// The static's name.
    pub name: String,
    /// The gate type's last path segment.
    pub ty: String,
    /// `true` for any visibility but private (`pub`, `pub(crate)`, ...).
    pub public: bool,
}

/// Scan a parsed source file for statics wrapped in a supervisor gate and
/// append them to `out`, walking inline modules.
pub fn scan_gate_statics(file: &syn::File, origin: &str, out: &mut Vec<GateStatic>) {
    fn walk(items: &[syn::Item], file: &str, out: &mut Vec<GateStatic>) {
        for item in items {
            match item {
                syn::Item::Static(s) => {
                    let syn::Type::Path(tp) = &*s.ty else {
                        continue;
                    };
                    let Some(last) = tp.path.segments.last() else {
                        continue;
                    };
                    let ty = last.ident.to_string();
                    if !matches!(ty.as_str(), "Backed" | "Leased" | "VetoGate") {
                        continue;
                    }
                    out.push(GateStatic {
                        file: file.to_string(),
                        line: s.ident.span().start().line,
                        name: s.ident.to_string(),
                        ty,
                        public: !matches!(s.vis, syn::Visibility::Inherited),
                    });
                }
                syn::Item::Mod(m) => {
                    if let Some((_, items)) = &m.content {
                        walk(items, file, out);
                    }
                }
                _ => {}
            }
        }
    }
    walk(&file.items, origin, out);
}

/// Where a file sits: its module path and what the path revealed about the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileModule {
    /// The module path of the file's top-level items.
    pub(crate) path: Vec<String>,
    /// `true` when the path was resolved under a `src/` component; `false` for
    /// the stem fallback (`t.rs`, `-`).
    pub(crate) rooted: bool,
    /// The directory above `src/`, `-` mapped to `_`, when the path has one
    /// (`common/src/x.rs` gives `common`).
    pub(crate) crate_dir: Option<String>,
}

/// Where a file's top-level items sit, from the file's path (see
/// [`scan_source`]).
pub(crate) fn file_module(origin: &str) -> FileModule {
    let path = std::path::Path::new(origin);
    let parts: Vec<&str> = path.iter().filter_map(|p| p.to_str()).collect();
    let Some(src) = parts.iter().rposition(|p| *p == "src") else {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str());
        let name = match dir {
            Some(dir) if matches!(stem, "mod" | "lib" | "main") => dir,
            _ => stem,
        };
        return FileModule {
            path: vec![name.to_string()],
            rooted: false,
            crate_dir: None,
        };
    };
    let crate_dir = (src > 0).then(|| parts[src - 1].replace('-', "_"));
    let mut rest = &parts[src + 1..];
    if let ["bin", tail @ ..] = rest
        && !tail.is_empty()
    {
        rest = &tail[1..];
    }
    let Some((file, dirs)) = rest.split_last() else {
        return FileModule {
            path: Vec::new(),
            rooted: true,
            crate_dir,
        };
    };
    let mut out: Vec<String> = dirs.iter().map(|d| (*d).to_string()).collect();
    match file.strip_suffix(".rs").unwrap_or(file) {
        "mod" => {}
        "main" | "lib" if dirs.is_empty() => {}
        stem => out.push(stem.to_string()),
    }
    FileModule {
        path: out,
        rooted: true,
        crate_dir,
    }
}

fn use_aliases(file: &syn::File) -> std::collections::BTreeMap<String, String> {
    fn collect(
        tree: &syn::UseTree,
        prefix: &mut Vec<String>,
        map: &mut std::collections::BTreeMap<String, String>,
    ) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                collect(&p.tree, prefix, map);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                let mut full = prefix.clone();
                full.push(n.ident.to_string());
                map.insert(n.ident.to_string(), full.join("::"));
            }
            syn::UseTree::Rename(r) => {
                let mut full = prefix.clone();
                full.push(r.ident.to_string());
                map.insert(r.rename.to_string(), full.join("::"));
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    collect(t, prefix, map);
                }
            }
            syn::UseTree::Glob(_) => {}
        }
    }
    fn walk(items: &[syn::Item], map: &mut std::collections::BTreeMap<String, String>) {
        for item in items {
            match item {
                syn::Item::Use(u) => collect(&u.tree, &mut Vec::new(), map),
                syn::Item::Mod(m) => {
                    if let Some((_, items)) = &m.content {
                        walk(items, map);
                    }
                }
                _ => {}
            }
        }
    }
    let mut map = std::collections::BTreeMap::new();
    walk(&file.items, &mut map);
    map
}

fn resolve_path(
    path: &str,
    aliases: &std::collections::BTreeMap<String, String>,
) -> (String, bool) {
    let (head, index) = match path.find('[') {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    let mut segs: Vec<&str> = head.split("::").collect();
    let explicit_self = segs.first() == Some(&"self");
    if explicit_self {
        segs.remove(0);
    }
    if let Some(full) = segs.first().and_then(|s| aliases.get(*s)) {
        let mut out = full.clone();
        for s in &segs[1..] {
            out.push_str("::");
            out.push_str(s);
        }
        return (format!("{out}{index}"), false);
    }
    let relative = explicit_self
        || segs.first() == Some(&"super")
        || match segs.as_slice() {
            [one] => *one != "crate",
            [first, _] => {
                *first != "crate" && first.chars().next().is_some_and(|c| c.is_lowercase())
            }
            _ => false,
        };
    (format!("{}{index}", segs.join("::")), relative)
}

/// Render `decl` as a diagram, dispatching to the lifecycle renderer if requested.
pub fn render(decl: &Decl, opts: &Options) -> String {
    if opts.states {
        states::render(decl, opts)
    } else {
        render::render(decl, opts)
    }
}

/// Resolve `compose_graph!` declarations by splicing in their fragments.
///
/// Returns the resolved declarations and any warnings (missing fragments,
/// ambiguous fragment names, and so on).
pub fn resolve(decls: Vec<Decl>) -> (Vec<Decl>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut consumed: Vec<(String, usize)> = Vec::new();
    let mut out: Vec<Decl> = Vec::new();

    for site in decls.iter().filter(|d| d.kind == DeclKind::Compose) {
        let mut items = Vec::new();
        let mut fragment_origins = std::collections::BTreeMap::new();
        for want in &site.fragments {
            let tail = want.rsplit("::").next().unwrap_or(want);
            let candidates: Vec<&Decl> = decls
                .iter()
                .filter(|d| d.kind == DeclKind::Fragment && d.name().as_deref() == Some(tail))
                .collect();
            let frag = match candidates.as_slice() {
                [] => {
                    warnings.push(format!(
                        "{}:{}: fragment `{want}` is not among the files given, so its \
                         nodes are drawn only where something depends on them; pass the \
                         file declaring it for the whole graph",
                        site.origin, site.line
                    ));
                    continue;
                }
                [one] => *one,
                many => {
                    warnings.push(format!(
                        "{}:{}: fragment `{want}` is ambiguous — {} declarations \
                         share the name ({}); none is spliced, each is drawn on \
                         its own. Rescan with only the intended file, or rename",
                        site.origin,
                        site.line,
                        many.len(),
                        many.iter()
                            .map(|d| format!("{}:{}", d.origin, d.line))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ));
                    continue;
                }
            };
            consumed.push((frag.origin.clone(), frag.line));
            match spliced(frag, want, tail) {
                Ok(mut resolved) => {
                    fragment_origins.insert(tail.to_string(), frag.origin.clone());
                    items.append(&mut resolved);
                }
                Err(e) => warnings.push(format!("{}:{}: {e}", frag.origin, frag.line)),
            }
        }
        items.extend(site.spec.items.iter().cloned());
        if let Some(ex) = &site.spec.default_executor {
            apply_default_executor(&mut items, ex);
        }
        let mut merged = clone_decl(site);
        merged.spec.items = items;
        merged.fragment_origins = fragment_origins;
        out.push(merged);
    }

    for d in &decls {
        let drawn_inside = d.kind == DeclKind::Compose
            || (d.kind == DeclKind::Fragment
                && consumed.iter().any(|(o, l)| *o == d.origin && *l == d.line));
        if !drawn_inside {
            out.push(clone_decl(d));
        }
    }
    out.sort_by_key(|d| (d.origin.clone(), d.line));
    (out, warnings)
}

fn spliced(frag: &Decl, want: &str, tail: &str) -> Result<Vec<Item>, Error> {
    let owner = match want.rsplit_once("::") {
        Some((head, _)) if !head.is_empty() => head,
        _ => "crate",
    };
    let owner: TokenStream = TokenStream::from_str(owner).unwrap_or_else(|_| quote!(crate));
    let body = substitute_dollar_crate(normalize_fragment_crate(frag.body.clone()), &owner);
    let err = |message: String| Error {
        file: frag.origin.clone(),
        line: frag.line,
        message,
    };
    let spec = find::parse_items(body, frag.kind, &err)?;
    Ok(spec
        .items
        .into_iter()
        .map(|mut item| {
            match &mut item {
                Item::Node(n) => n.fragment = Some(tail.to_string()),
                Item::Pool(p) => p.fragment = Some(tail.to_string()),
                Item::Executor(_) => {}
            }
            item
        })
        .collect())
}

/// Remove the named nodes and pools from a declaration, along with any deps on them.
///
/// Returns the names that were not present in the declaration.
pub fn exclude(decl: &mut Decl, names: &[String]) -> Vec<String> {
    let present: Vec<String> = decl.spec.items.iter().filter_map(item_name).collect();
    let unmatched: Vec<String> = names
        .iter()
        .filter(|n| !present.contains(n))
        .cloned()
        .collect();
    decl.spec
        .items
        .retain(|i| item_name(i).is_none_or(|n| !names.contains(&n)));
    for item in &mut decl.spec.items {
        let deps = match item {
            Item::Node(n) => &mut n.deps,
            Item::Pool(p) => &mut p.deps,
            Item::Executor(_) => continue,
        };
        deps.retain(|d| !names.contains(&d.ident.to_string()));
    }
    unmatched
}

fn item_name(item: &Item) -> Option<String> {
    match item {
        Item::Node(n) => Some(n.ident.to_string()),
        Item::Pool(p) => Some(p.ident.to_string()),
        Item::Executor(_) => None,
    }
}

fn clone_decl(d: &Decl) -> Decl {
    Decl {
        kind: d.kind,
        spec: d.spec.clone(),
        body: d.body.clone(),
        fragments: d.fragments.clone(),
        fragment_origins: d.fragment_origins.clone(),
        origin: d.origin.clone(),
        line: d.line,
    }
}

/// A `#[dataflow]` fn found in a scanned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFn {
    /// Its module path: the file's (see [`scan_source`]), then the inline `mod`s around it.
    pub module: Vec<String>,
    /// `true` when the file's path was resolved under a `src/` component;
    /// `false` for the stem fallback (`t.rs`, `-`).
    pub rooted: bool,
    /// The directory above `src/`, `-` mapped to `_`, when the file's path has
    /// one (`common/src/x.rs` gives `common`).
    pub crate_dir: Option<String>,
    /// Its name.
    pub name: String,
}

impl std::fmt::Display for ScannedFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.module.is_empty() {
            write!(f, "crate::{}", self.name)
        } else {
            write!(f, "{}::{}", self.module.join("::"), self.name)
        }
    }
}

/// Find every `#[dataflow]` fn in a parsed file, inline modules included, and
/// append it to `out` with its module path.
pub fn scan_fns(file: &syn::File, origin: &str, out: &mut Vec<ScannedFn>) {
    fn walk(
        items: &[syn::Item],
        module: &mut Vec<String>,
        rooted: bool,
        crate_dir: Option<&String>,
        out: &mut Vec<ScannedFn>,
    ) {
        for item in items {
            match item {
                syn::Item::Fn(f)
                    if f.attrs
                        .iter()
                        .any(embassy_supervisor_syntax::is_dataflow_attr) =>
                {
                    out.push(ScannedFn {
                        module: module.clone(),
                        rooted,
                        crate_dir: crate_dir.cloned(),
                        name: f.sig.ident.to_string(),
                    });
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content {
                        module.push(m.ident.to_string());
                        walk(inner, module, rooted, crate_dir, out);
                        module.pop();
                    }
                }
                _ => {}
            }
        }
    }
    let FileModule {
        mut path,
        rooted,
        crate_dir,
    } = file_module(origin);
    walk(&file.items, &mut path, rooted, crate_dir.as_ref(), out);
}

/// A `#[dataflow_bundle]` module and the `#[dataflow]` functions it contains.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// The bundle module's path: the file's module path, then the inline
    /// `mod`s down to and including the bundle's own.
    pub path: Vec<String>,
    /// `true` when the file's path was resolved under a `src/` component;
    /// `false` for the stem fallback (`t.rs`, `-`).
    pub rooted: bool,
    /// The directory above `src/`, `-` mapped to `_`, when the file's path has
    /// one (`common/src/x.rs` gives `common`).
    pub crate_dir: Option<String>,
    /// The bundle name given in the attribute, or `"BUNDLE"` if empty.
    pub name: String,
    /// The names of the `#[dataflow]` functions inside the module.
    pub fns: Vec<String>,
}

/// Scan a parsed source file for `#[dataflow_bundle]` modules and append them to `out`.
pub fn scan_bundles(file: &syn::File, origin: &str, out: &mut Vec<Bundle>) {
    fn walk(
        items: &[syn::Item],
        module: &mut Vec<String>,
        rooted: bool,
        crate_dir: Option<&String>,
        out: &mut Vec<Bundle>,
    ) {
        for item in items {
            let syn::Item::Mod(m) = item else { continue };
            let Some((_, inner)) = &m.content else {
                continue;
            };
            module.push(m.ident.to_string());
            if let Some(attr) = m.attrs.iter().find(|a| {
                a.path()
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "dataflow_bundle")
            }) {
                let name = match &attr.meta {
                    syn::Meta::List(l) => l.tokens.to_string(),
                    _ => String::new(),
                };
                let name = if name.is_empty() {
                    "BUNDLE".to_string()
                } else {
                    name
                };
                let fns = inner
                    .iter()
                    .filter_map(|i| match i {
                        syn::Item::Fn(f)
                            if f.attrs
                                .iter()
                                .any(embassy_supervisor_syntax::is_dataflow_attr) =>
                        {
                            Some(f.sig.ident.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                out.push(Bundle {
                    path: module.clone(),
                    rooted,
                    crate_dir: crate_dir.cloned(),
                    name,
                    fns,
                });
            }
            walk(inner, module, rooted, crate_dir, out);
            module.pop();
        }
    }
    let FileModule {
        mut path,
        rooted,
        crate_dir,
    } = file_module(origin);
    walk(&file.items, &mut path, rooted, crate_dir.as_ref(), out);
}

/// Replace each binding that names a bundle with bindings of the bundle's
/// member fns, which keep its `#[cfg]`; other bindings pass through.
pub(crate) fn expand_bundles(bindings: Vec<FnBinding>, bundles: &[Bundle]) -> Vec<FnBinding> {
    bindings
        .into_iter()
        .flat_map(|b| {
            match bundles.iter().find(|bundle| {
                bundle.name == b.name
                    && b.module_matches(&bundle.path, bundle.rooted, bundle.crate_dir.as_deref())
            }) {
                Some(bundle) => bundle
                    .fns
                    .iter()
                    .map(|f| FnBinding {
                        name: f.clone(),
                        module: bundle.path.clone(),
                        absolute: true,
                        cfg: b.cfg.clone(),
                    })
                    .collect(),
                None => vec![b],
            }
        })
        .collect()
}

/// A fn a declaration binds for its dataflow: an item's `task:`/`spawn:` fn
/// under `discover`, or one of its `dataflow:` adoptions.
#[derive(Clone, Debug)]
pub(crate) struct FnBinding {
    /// The fn's name: the path's last segment.
    pub(crate) name: String,
    /// The path's module segments as written, `crate`, `self` and `super` dropped.
    pub(crate) module: Vec<String>,
    /// Written from `crate::`, so no crate name can precede `module`.
    pub(crate) absolute: bool,
    /// The `#[cfg]` texts on the `discover` clause or the adoption.
    pub(crate) cfg: Vec<String>,
}

impl FnBinding {
    fn from_path(path: &syn::Path, cfg: &[syn::Attribute]) -> Option<Self> {
        let mut module: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let name = module.pop()?;
        let absolute = module.first().is_some_and(|s| s == "crate");
        module.retain(|s| !matches!(s.as_str(), "crate" | "self" | "super"));
        Some(Self {
            name,
            module,
            absolute,
            cfg: model::cfg_texts(cfg),
        })
    }

    /// Can a scanned fn at `module` be the one this binding names?
    /// * written `crate::` and the file is under `src/`: the paths must be equal;
    /// * otherwise the binding's segments must end the module path;
    /// * or, for a path not written `crate::`, one leading segment more names the crate
    ///   (`common::tasks::rate::entry` for `common/src/tasks/rate.rs`). That form reaches a
    ///   crate-root fn only when the file's crate directory is known and equals that segment,
    ///   so `imu::run` never binds the root `fn run` beside it.
    fn module_matches(&self, module: &[String], rooted: bool, crate_dir: Option<&str>) -> bool {
        if self.absolute && rooted {
            return module == self.module;
        }
        if module.ends_with(&self.module) {
            return true;
        }
        !self.absolute
            && self.module.len() == module.len() + 1
            && self.module[1..] == *module
            && (!module.is_empty() || crate_dir == Some(self.module[0].as_str()))
    }

    fn binds(&self, f: &ScannedFn) -> bool {
        self.name == f.name && self.module_matches(&f.module, f.rooted, f.crate_dir.as_deref())
    }
}

impl std::fmt::Display for FnBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for m in &self.module {
            write!(f, "{m}::")?;
        }
        f.write_str(&self.name)
    }
}

/// The fns `item` binds. Its task fn counts only under `discover`: that is
/// the only case the macro takes the fn's tables.
pub(crate) fn fn_bindings(item: &Item) -> Vec<FnBinding> {
    let (discover, source, adopted) = match item {
        Item::Node(n) => (n.discover.as_ref(), n.source.as_ref(), &n.dataflow),
        Item::Pool(p) => (p.discover.as_ref(), Some(&p.source), &p.dataflow),
        Item::Executor(_) => return Vec::new(),
    };
    let task = discover
        .zip(source)
        .and_then(|(d, TaskSource::Shell(e) | TaskSource::Spawn(e))| {
            Some((callee_path(e)?, d.cfg.as_slice()))
        });
    task.into_iter()
        .chain(adopted.iter().map(|a| (&a.path, a.cfg.as_slice())))
        .filter_map(|(path, cfg)| FnBinding::from_path(path, cfg))
        .collect()
}

fn callee_path(e: &syn::Expr) -> Option<&syn::Path> {
    match e {
        syn::Expr::Path(p) => Some(&p.path),
        syn::Expr::Call(c) => callee_path(&c.func),
        _ => None,
    }
}

/// The binding `d` came through, if one of `bindings` names its fn; an
/// ungated binding wins over a gated one.
pub(crate) fn binding_for<'a>(bindings: &'a [FnBinding], d: &Discovered) -> Option<&'a FnBinding> {
    bindings
        .iter()
        .filter(|b| {
            b.name == d.access.func && b.module_matches(&d.module, d.rooted, d.crate_dir.as_deref())
        })
        .min_by_key(|b| b.cfg.len())
}

/// Check that every `discover` / `dataflow:` binding has a scanned function,
/// and that every scanned `#[dataflow]` function is bound.
///
/// Returns human-readable warnings for mismatches.
pub fn coverage_warnings(
    decls: &[Decl],
    scanned: &[ScannedFn],
    dep_scanned: &[ScannedFn],
    bundles: &[Bundle],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut used: Vec<bool> = vec![false; scanned.len()];
    for item in decls.iter().flat_map(|d| &d.spec.items) {
        let Some(node) = item_name(item) else {
            continue;
        };
        for b in expand_bundles(fn_bindings(item), bundles) {
            let mut any = false;
            for (i, sf) in scanned.iter().enumerate() {
                if b.binds(sf) {
                    used[i] = true;
                    any = true;
                }
            }
            any |= dep_scanned.iter().any(|sf| b.binds(sf));
            if !any {
                warnings.push(format!(
                    "{node}: no `#[dataflow]` fn `{b}` among the scanned files — \
                     its derived edges will not draw; pass the file that defines it"
                ));
            }
        }
    }
    for (i, sf) in scanned.iter().enumerate() {
        if !used[i] {
            warnings.push(format!(
                "`#[dataflow]` fn `{sf}` is not bound by any \
                 declaration (no `discover` task or `dataflow:` adoption names it)"
            ));
        }
    }
    warnings
}

/// Build a JSON representation of the declarations and discovered accesses.
///
/// This is the machine-readable model `supervisor-mermaid --json` prints, for
/// tooling that is not a diagram. It projects [`full_model`] field for field.
pub fn model_json(decls: &[Decl], discovered: &[Discovered]) -> serde_json::Value {
    use model::{ItemModel, TaskKind};
    let lit_json = |l: &Option<model::LitValue>| match l {
        Some(v) => match v.as_u64() {
            Some(n) => serde_json::json!(n),
            None => serde_json::json!(v.text),
        },
        None => serde_json::Value::Null,
    };
    let lit_cfg = |l: &Option<model::LitValue>| -> Vec<String> {
        l.as_ref().map(|v| v.cfg.clone()).unwrap_or_default()
    };
    let dep_json = |d: &model::DepModel| {
        serde_json::json!({
            "name": d.name,
            "ready": d.ready,
            "bound": d.bound,
            "cfg": d.cfg,
        })
    };
    let sig_json = |s: &model::SignalModel| {
        serde_json::json!({
            "path": s.path,
            "observed": s.observed,
            "beat": s.beat,
            "veto": s.veto,
            "via": s.via,
            "cfg": s.cfg,
        })
    };
    let res_json = |r: &model::ResourceModel| {
        serde_json::json!({
            "name": r.name,
            "ty": r.ty.as_ref().map(|t| t.text.clone()),
            "local": r.local,
            "consume": r.consume,
            "shared": r.shared,
            "divisible": r.divisible,
            "serialized": r.serialized,
            "cfg": r.cfg,
        })
    };
    let task_json = |t: &model::TaskModel| {
        serde_json::json!({
            "kind": match t.kind { TaskKind::Spawn => "spawn", TaskKind::Shell => "task" },
            "path": t.path,
        })
    };
    let state_json = |st: &Option<model::StateModel>| match st {
        Some(v) => serde_json::json!({ "ty": v.ty.text, "init": v.init }),
        None => serde_json::Value::Null,
    };
    let full = full_model(decls);
    let graphs: Vec<serde_json::Value> = full
        .graphs
        .iter()
        .map(|g| {
            let items: Vec<serde_json::Value> = g
                .items
                .iter()
                .map(|item| match item {
                    ItemModel::Node(n) => serde_json::json!({
                        "kind": "node",
                        "name": n.name,
                        "mode": n.mode,
                        "cfg": n.cfg,
                        "deps": n.deps.iter().map(dep_json).collect::<Vec<_>>(),
                        "executor": n.executor,
                        "executor_defaulted": n.executor_defaulted,
                        "resources": n.resources.iter().map(res_json).collect::<Vec<_>>(),
                        "provides": n.provides.iter().map(|p| serde_json::json!({
                            "name": p.name,
                            "cfg": p.cfg,
                        })).collect::<Vec<_>>(),
                        "discover": n.discover.is_some(),
                        "discover_cfg": n.discover.clone().unwrap_or_default(),
                        "dataflow": n.dataflow.iter().map(|a| &a.path).collect::<Vec<_>>(),
                        "dataflow_cfg": n.dataflow.iter().map(|a| &a.cfg).collect::<Vec<_>>(),
                        "reads": n.reads.iter().map(sig_json).collect::<Vec<_>>(),
                        "writes": n.writes.iter().map(sig_json).collect::<Vec<_>>(),
                        "disabled": n.disabled.is_some(),
                        "disabled_cfg": n.disabled.clone().unwrap_or_default(),
                        "parked": n.task.is_none(),
                        "task": n.task.as_ref().map(task_json),
                        "slot_timeout_ms": lit_json(&n.slot_timeout_ms),
                        "slot_timeout_cfg": lit_cfg(&n.slot_timeout_ms),
                        "ack_timeout_ms": lit_json(&n.ack_timeout_ms),
                        "ack_timeout_cfg": lit_cfg(&n.ack_timeout_ms),
                        "beat_timeout_ms": lit_json(&n.beat_timeout_ms),
                        "beat_timeout_cfg": lit_cfg(&n.beat_timeout_ms),
                        "beat_window": lit_json(&n.beat_window),
                        "beat_window_cfg": lit_cfg(&n.beat_window),
                        "ready_on_write": n.ready_on_write.is_some(),
                        "ready_on_write_cfg": n.ready_on_write.clone().unwrap_or_default(),
                        "enabled_if": n.enabled_if.as_ref().map(|e| e.predicate.text.clone()),
                        "enabled_if_cfg": n.enabled_if.as_ref().map(|e| e.cfg.clone()).unwrap_or_default(),
                        "restart_budget": n.restart_budget.as_ref().map(|b| serde_json::json!({
                            "max": b.max.as_u64(),
                            "window_ms": b.window_ms.as_u64(),
                        })),
                        "restart_budget_cfg": n.restart_budget.as_ref().map(|b| b.cfg.clone()).unwrap_or_default(),
                        "exit": n.exit.as_ref().map(|t| t.text.clone()),
                        "state": state_json(&n.state),
                        "cancel": n.cancel,
                    }),
                    ItemModel::Pool(p) => serde_json::json!({
                        "kind": "pool",
                        "name": p.name,
                        "modes": p.modes,
                        "cfg": p.cfg,
                        "deps": p.deps.iter().map(dep_json).collect::<Vec<_>>(),
                        "executor": p.executor,
                        "executor_defaulted": p.executor_defaulted,
                        "resources": p.resources.iter().map(res_json).collect::<Vec<_>>(),
                        "discover": p.discover.is_some(),
                        "discover_cfg": p.discover.clone().unwrap_or_default(),
                        "dataflow": p.dataflow.iter().map(|a| &a.path).collect::<Vec<_>>(),
                        "dataflow_cfg": p.dataflow.iter().map(|a| &a.cfg).collect::<Vec<_>>(),
                        "reads": p.reads.iter().map(sig_json).collect::<Vec<_>>(),
                        "writes": p.writes.iter().map(sig_json).collect::<Vec<_>>(),
                        "task": task_json(&p.task),
                        "policy": p.policy.text,
                        "policy_ty": p.policy_ty.as_ref().map(|t| t.text.clone()),
                        "min": p.min.text,
                        "max": p.max.text,
                        "slot_timeout_ms": lit_json(&p.slot_timeout_ms),
                        "slot_timeout_cfg": lit_cfg(&p.slot_timeout_ms),
                        "ack_timeout_ms": lit_json(&p.ack_timeout_ms),
                        "ack_timeout_cfg": lit_cfg(&p.ack_timeout_ms),
                        "state": state_json(&p.state),
                        "cancel": p.cancel,
                    }),
                    ItemModel::Executor(x) => serde_json::json!({
                        "kind": "executor",
                        "name": x.name,
                        "default": x.default,
                        "cfg": x.cfg,
                    }),
                })
                .collect();
            serde_json::json!({
                "macro": g.macro_name,
                "name": g.name,
                "origin": g.origin,
                "line": g.line,
                "observe_writes": g.observe_writes.as_ref().map(|e| e.text.clone()),
                "observe_reads": g.observe_reads.as_ref().map(|e| e.text.clone()),
                "items": items,
            })
        })
        .collect();
    let accesses: Vec<serde_json::Value> = discovered
        .iter()
        .map(|d| {
            serde_json::json!({
                "module": d.module.join("::"),
                "fn": d.access.func,
                "verb": d.access.verb,
                "write": d.access.write,
                "path": d.access.path,
                "cfg": d.access.cfgs,
            })
        })
        .collect();
    serde_json::json!({ "graphs": graphs, "discovered": accesses })
}

#[cfg(test)]
mod tests {
    use super::*;
    use embassy_supervisor_syntax::TaskSource;

    const GRAPH: &str = r#"
        embassy_supervisor::supervisor_graph! {
            executor HIGH;
            observe writes: it.load(core::sync::atomic::Ordering::Relaxed);
            node WATCHDOG = Terminate, deps: [], task: crate::wd::task,
                resources: [WD: embassy_rp::watchdog::Watchdog];
            node HEARTBEAT = Pause, deps: [WATCHDOG], executor: HIGH,
                task: crate::hb::task, beat_timeout: 15000,
                reads: [crate::hb::PERIOD observed via crate::hb::READS.load(X)],
                writes: [crate::hb::BLINKS observed beat];
            node OTA = Terminate, deps: [NET ready, WATCHDOG], task: crate::ota::task;
            #[cfg(feature = "extra")]
            node EXTRA = Terminate, deps: [HEARTBEAT ready bound];
        }
    "#;

    fn graph() -> Decl {
        let mut d = parse_source(GRAPH, "t.rs").unwrap();
        assert_eq!(d.len(), 1);
        d.remove(0)
    }

    fn node<'a>(d: &'a Decl, name: &str) -> &'a embassy_supervisor_syntax::NodeItem {
        d.spec
            .items
            .iter()
            .find_map(|i| match i {
                Item::Node(n) if n.ident == name => Some(n),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no node {name}"))
    }

    fn parsed(src: &str) -> syn::File {
        syn::parse_file(src).expect("the test source parses")
    }

    #[test]
    fn the_shared_parser_yields_the_whole_declaration() {
        let g = graph();
        let hb = node(&g, "HEARTBEAT");
        assert_eq!(hb.mode, "Pause");
        assert!(matches!(hb.source, Some(TaskSource::Shell(_))));
        assert_eq!(hb.executor.as_ref().unwrap(), "HIGH");
        assert_eq!(hb.beat_timeout.as_ref().unwrap().value.to_string(), "15000");
        assert!(hb.writes[0].observed.is_some() && hb.writes[0].beat.is_some());
        // `observed via <expr>` parses as a real expression, not a token blob.
        assert!(hb.reads[0].observed.is_some() && hb.reads[0].via.is_some());
    }

    #[test]
    fn the_dep_markers_survive_parsing() {
        let g = graph();
        let deps: Vec<_> = g
            .spec
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Node(n) => Some(&n.deps),
                _ => None,
            })
            .flatten()
            .map(|d| (d.ident.to_string(), d.ready.is_some(), d.bound.is_some()))
            .collect();
        assert!(deps.contains(&("NET".into(), true, false)));
        assert!(deps.contains(&("WATCHDOG".into(), false, false)));
        assert!(deps.contains(&("HEARTBEAT".into(), true, true)));
    }

    #[test]
    fn a_cfg_gated_node_is_kept_and_marked() {
        // A proc-macro cannot evaluate cfg and neither can this: the node is
        // drawn like any other, the predicate in its label.
        let g = graph();
        assert!(!node(&g, "EXTRA").cfg.is_empty());
        assert!(node(&g, "EXTRA").source.is_none(), "parked");
    }

    #[test]
    fn a_dep_leaving_the_declaration_becomes_an_external_box() {
        // NET is declared nowhere here, which is exactly a fragment's situation.
        let out = render(&graph(), &Options::default());
        assert!(out.contains("n_NET([\"NET\"])"), "{out}");
        assert!(out.contains("class n_NET external;"), "{out}");
    }

    #[test]
    fn each_dep_marker_draws_a_different_edge() {
        let out = render(&graph(), &Options::default());
        assert!(
            out.contains("n_HEARTBEAT == \"ready bound\" ==> n_EXTRA"),
            "{out}"
        );
        assert!(out.contains("n_NET -- \"ready\" --> n_OTA"), "{out}");
        assert!(out.contains("n_WATCHDOG --> n_HEARTBEAT"), "{out}");
    }

    #[test]
    fn coupling_routes_through_a_signal_box_never_node_to_node() {
        let out = render(
            &graph(),
            &Options {
                signals: true,
                ..Default::default()
            },
        );
        assert!(out.contains("[/\"BLINKS\"/]"), "{out}");
        assert!(
            out.contains("n_HEARTBEAT -. \"observed · beat\" .-> s_crate__hb__BLINKS"),
            "{out}"
        );
        assert!(
            out.contains("s_crate__hb__PERIOD -. \"observed\" .-> n_HEARTBEAT"),
            "{out}"
        );
    }

    #[test]
    fn signals_stay_out_of_the_picture_unless_asked_for() {
        assert!(!render(&graph(), &Options::default()).contains("BLINKS"));
    }

    #[test]
    fn discovered_accesses_draw_from_the_body() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: worker, discover;
                node B = Terminate, deps: [], writes: [crate::cfg::PERIOD];
            }
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode) {
                let ms = node.get(&PERIOD);
                node.put(&crate::OUT, ms);
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        assert_eq!(discovered.len(), 2, "{discovered:?}");
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered: discovered.clone(),
                ..Default::default()
            },
        );
        assert!(
            text.contains("-- \"discovered\" --> n_A"),
            "read edge into the node: {text}"
        );
        assert!(
            text.contains("n_A -- \"discovered\" -->"),
            "the derived write draws out of the node: {text}"
        );
        assert!(
            text.contains("s_crate__cfg__PERIOD -- \"discovered\" --> n_A"),
            "the short call-site path reuses the declared signal's box: {text}"
        );

        let signals = render(
            &d[0],
            &Options {
                signals: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            signals.contains("s_crate__cfg__PERIOD -. \"discovered\" .-> n_A"),
            "{signals}"
        );
        assert!(
            signals.contains("n_A -. \"discovered\" .-> s_crate__OUT"),
            "{signals}"
        );
    }

    #[test]
    fn adopted_accessors_attribute_to_the_adopter() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], dataflow: [crate::cfg::set_period];
            }
            pub mod cfg {
                #[embassy_supervisor::dataflow]
                pub fn set_period(node: &'static TaskNode, ms: i32) {
                    node.put(&PERIOD, ms);
                }
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered: discovered.clone(),
                ..Default::default()
            },
        );
        assert!(
            text.contains("n_A -- \"discovered\" --> s_t__cfg__PERIOD"),
            "the accessor's write draws on the adopter, its box keyed by the \
             defining module: {text}"
        );

        let signals = render(
            &d[0],
            &Options {
                signals: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            signals.contains("n_A -. \"discovered\" .-> s_t__cfg__PERIOD"),
            "{signals}"
        );
    }

    #[test]
    fn the_access_walk_threads_inline_modules() {
        let source = r#"
            #[cfg(feature = "a")]
            #[embassy_supervisor::dataflow]
            async fn top(node: &'static TaskNode) {
                node.put(&crate::X, 1);
                #[cfg(feature = "b")]
                let _ = node.get(&crate::Y);
                node.put(&crate::X, 2);
            }
            mod inner {
                #[cfg_attr(feature = "c", embassy_supervisor::dataflow)]
                pub fn wrapped(node: &'static TaskNode) {
                    node.put(&crate::Z, 1);
                }
                struct S;
                impl S {
                    #[embassy_supervisor::dataflow]
                    fn method(&self, node: &'static TaskNode) {
                        let _ = node.open(&crate::W);
                    }
                }
            }
            fn plain(node: &'static TaskNode) {
                node.put(&crate::NOT_SCANNED, 1);
            }
        "#;
        let mut found = Vec::new();
        scan_source(&parsed(source), "t.rs", &mut found);
        let accesses: Vec<Access> = found.iter().map(|d| d.access.clone()).collect();
        assert_eq!(accesses.len(), 4, "{accesses:?}");
        let modules: Vec<String> = found.iter().map(|d| d.module.join("::")).collect();
        assert_eq!(modules, ["t", "t", "t::inner", "t::inner"]);
    }

    #[test]
    fn same_named_fns_attribute_by_module() {
        let graph = r#"
            embassy_supervisor::supervisor_graph! {
                node RATE = Terminate, deps: [], task: common::tasks::rate::entry, discover;
                node ESKF = Terminate, deps: [], task: common::tasks::eskf::entry, discover;
            }
        "#;
        let rate = r#"
            use crate::signals as sig;
            #[embassy_supervisor::dataflow]
            async fn entry(node: &'static TaskNode) {
                let _ = node.open(&sig::EST).await;
            }
        "#;
        let eskf = r#"
            use crate::signals as s;
            #[embassy_supervisor::dataflow]
            async fn entry(node: &'static TaskNode) {
                node.put(&s::EST, 1);
            }
        "#;
        let d = parse_source(graph, "src/main.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(rate), "src/tasks/rate.rs", &mut discovered);
        scan_source(&parsed(eskf), "src/tasks/eskf.rs", &mut discovered);
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            text.contains("s_crate__signals__EST -- \"gated\" --> n_RATE"),
            "rate's own entry opens the estimate: {text}"
        );
        assert!(
            text.contains("n_ESKF -- \"discovered\" --> s_crate__signals__EST"),
            "eskf's own entry writes it: {text}"
        );
        assert!(
            !text.contains("\"gated\" --> n_ESKF"),
            "eskf's entry never opens anything: {text}"
        );
        assert_eq!(
            text.matches("s_crate__signals__EST[").count(),
            1,
            "two aliases, one box: {text}"
        );
    }

    #[test]
    fn a_files_module_path_comes_from_its_place_under_src() {
        let m = |o: &str| file_module(o).path.join("::");
        for root in [
            "fw/src/main.rs",
            "fw/src/lib.rs",
            "fw/src/bin/tool.rs",
            "fw/src/bin/tool/main.rs",
        ] {
            assert_eq!(m(root), "", "{root} is a crate root");
        }
        assert_eq!(m("fw/src/imu/driver.rs"), "imu::driver");
        assert_eq!(m("fw/src/imu/mod.rs"), "imu");
        assert_eq!(m("fw/src/bin/tool/args.rs"), "args");
        assert_eq!(m("sensors.rs"), "sensors");
        assert_eq!(m("inline/main.rs"), "inline");
        assert_eq!(m("-"), "-");
        assert_eq!(
            file_module("common/src/tasks/rate.rs").crate_dir.as_deref(),
            Some("common")
        );
        assert_eq!(
            file_module("my-fw/src/main.rs").crate_dir.as_deref(),
            Some("my_fw")
        );
        assert!(!file_module("t.rs").rooted);
    }

    #[test]
    fn inline_module_fns_bind_by_their_full_path() {
        let graph = r#"
            embassy_supervisor::supervisor_graph! {
                node PUB = Terminate, task: sensors::estimate::publish, discover;
                node SUB = Terminate, task: sensors::estimate::subscribe, discover;
            }
        "#;
        let sensors = parsed(
            r#"
            pub mod estimate {
                static ESTIMATE: Backed<Watch<u32>> = Backed::new(Watch::new());
                #[dataflow]
                pub async fn publish(node: &'static TaskNode, e: u32) {
                    node.writer(&ESTIMATE).send(e);
                }
                #[dataflow]
                pub async fn subscribe(node: &'static TaskNode) {
                    let _ = node.open(&ESTIMATE).await;
                }
            }
        "#,
        );
        let d = parse_source(graph, "src/main.rs").unwrap();
        let mut fns = Vec::new();
        scan_fns(&sensors, "src/sensors.rs", &mut fns);
        assert_eq!(fns[0].module, ["sensors", "estimate"]);
        let coverage = coverage_warnings(&d, &fns, &[], &[]);
        assert!(coverage.is_empty(), "{coverage:?}");

        let mut discovered = Vec::new();
        scan_source(&sensors, "src/sensors.rs", &mut discovered);
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered: discovered.clone(),
                ..Default::default()
            },
        );
        assert!(
            text.contains("n_PUB -- \"discovered\" --> s_sensors__estimate__ESTIMATE"),
            "{text}"
        );
        assert!(
            text.contains("s_sensors__estimate__ESTIMATE -- \"gated\" --> n_SUB"),
            "{text}"
        );
        let lints = dataflow_lints(&d, &discovered, &[], &LintCats::all(), &[]);
        assert!(lints.is_empty(), "{lints:?}");
    }

    #[test]
    fn same_named_fns_in_same_named_files_bind_by_their_directories() {
        let graph = r#"
            embassy_supervisor::supervisor_graph! {
                node IMU = Terminate, task: crate::imu::driver::run, discover;
                node BARO = Terminate, task: crate::baro::driver::run, discover;
            }
        "#;
        let imu = "#[dataflow]\npub async fn run(node: &'static TaskNode) { node.put(&crate::sig::ACCEL, 1); }";
        let baro = "#[dataflow]\npub async fn run(node: &'static TaskNode) { node.put(&crate::sig::PRESSURE, 1); }";
        let d = parse_source(graph, "src/main.rs").unwrap();
        let (mut fns, mut discovered) = (Vec::new(), Vec::new());
        for (src, origin) in [(imu, "src/imu/driver.rs"), (baro, "src/baro/driver.rs")] {
            let file = parsed(src);
            scan_fns(&file, origin, &mut fns);
            scan_source(&file, origin, &mut discovered);
        }
        let coverage = coverage_warnings(&d, &fns, &[], &[]);
        assert!(coverage.is_empty(), "{coverage:?}");
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            text.contains("n_IMU -- \"discovered\" --> s_crate__sig__ACCEL"),
            "{text}"
        );
        assert!(
            text.contains("n_BARO -- \"discovered\" --> s_crate__sig__PRESSURE"),
            "{text}"
        );
        assert!(
            !text.contains("n_IMU -- \"discovered\" --> s_crate__sig__PRESSURE"),
            "{text}"
        );
        assert!(
            !text.contains("n_BARO -- \"discovered\" --> s_crate__sig__ACCEL"),
            "{text}"
        );
    }

    #[test]
    fn the_graph_files_own_fns_bind_only_by_their_path() {
        let rooted = r#"
            embassy_supervisor::supervisor_graph! {
                node IMU = Terminate, task: crate::imu::run, discover;
            }
            #[embassy_supervisor::dataflow]
            async fn run(node: &'static TaskNode) { node.put(&crate::sig::LOCAL, 1); }
        "#;
        let relative = rooted.replace("task: crate::imu::run", "task: imu::run");
        let imu = "#[dataflow]\npub async fn run(node: &'static TaskNode) { node.put(&crate::sig::ACCEL, 1); }";
        for main in [rooted, relative.as_str()] {
            let d = parse_source(main, "src/main.rs").unwrap();
            let (mut fns, mut discovered) = (Vec::new(), Vec::new());
            for (src, origin) in [(main, "src/main.rs"), (imu, "src/imu.rs")] {
                let file = parsed(src);
                scan_fns(&file, origin, &mut fns);
                scan_source(&file, origin, &mut discovered);
            }
            let text = render(
                &d[0],
                &Options {
                    runtime: true,
                    discovered,
                    ..Default::default()
                },
            );
            assert!(
                text.contains("n_IMU -- \"discovered\" --> s_crate__sig__ACCEL"),
                "{main}\n{text}"
            );
            assert!(!text.contains("LOCAL"), "{main}\n{text}");
            let coverage = coverage_warnings(&d, &fns, &[], &[]);
            assert_eq!(coverage.len(), 1, "{main}\n{coverage:?}");
            assert!(
                coverage[0].contains("`crate::run` is not bound"),
                "{main}\n{coverage:?}"
            );
        }
    }

    #[test]
    fn a_crate_root_fn_binds_through_the_crate_dir() {
        let by_crate = r#"
            embassy_supervisor::supervisor_graph! {
                node RUN = Terminate, task: fw::run, discover;
            }
            #[embassy_supervisor::dataflow]
            async fn run(node: &'static TaskNode) { node.put(&crate::sig::OUT, 1); }
        "#;
        let by_module = by_crate.replace("task: fw::run", "task: imu::run");
        let scan = |main: &str| {
            let origin = "fw/src/main.rs";
            let d = parse_source(main, origin).unwrap();
            let file = parsed(main);
            let (mut fns, mut discovered) = (Vec::new(), Vec::new());
            scan_fns(&file, origin, &mut fns);
            scan_source(&file, origin, &mut discovered);
            let text = render(
                &d[0],
                &Options {
                    runtime: true,
                    discovered,
                    ..Default::default()
                },
            );
            (text, coverage_warnings(&d, &fns, &[], &[]))
        };

        let (text, coverage) = scan(by_crate);
        assert!(
            text.contains("n_RUN -- \"discovered\" --> s_crate__sig__OUT"),
            "the crate's own name reaches its root fn: {text}"
        );
        assert!(coverage.is_empty(), "{coverage:?}");

        let (text, coverage) = scan(&by_module);
        assert!(!text.contains("OUT"), "{text}");
        assert_eq!(coverage.len(), 2, "{coverage:?}");
        assert!(
            coverage[0].contains("RUN: no `#[dataflow]` fn `imu::run`"),
            "{coverage:?}"
        );
        assert!(
            coverage[1].contains("`crate::run` is not bound"),
            "{coverage:?}"
        );
    }

    #[test]
    fn a_rooted_crate_binding_matches_exactly() {
        let main = r#"
            embassy_supervisor::supervisor_graph! {
                node TOP = Terminate, task: crate::run, discover;
            }
            #[embassy_supervisor::dataflow]
            fn run(node: &'static TaskNode) { node.put(&crate::A, 1); }
        "#;
        let imu = "#[dataflow]\nfn run(node: &'static TaskNode) { node.put(&crate::B, 1); }";
        let d = parse_source(main, "src/main.rs").unwrap();
        let (mut fns, mut discovered) = (Vec::new(), Vec::new());
        for (src, origin) in [(main, "src/main.rs"), (imu, "src/sensors/imu.rs")] {
            let file = parsed(src);
            scan_fns(&file, origin, &mut fns);
            scan_source(&file, origin, &mut discovered);
        }
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            text.contains("n_TOP -- \"discovered\" --> s_crate__A"),
            "{text}"
        );
        assert!(
            !text.contains("s_crate__B"),
            "`crate::run` names the root fn only: {text}"
        );
        let coverage = coverage_warnings(&d, &fns, &[], &[]);
        assert_eq!(coverage.len(), 1, "{coverage:?}");
        assert!(
            coverage[0].contains("`sensors::imu::run` is not bound"),
            "{coverage:?}"
        );
    }

    #[test]
    fn a_task_fn_binds_only_under_discover() {
        let body = parsed(
            r#"
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode) { node.put(&crate::OUT, 1); }
        "#,
        );
        let (mut fns, mut discovered) = (Vec::new(), Vec::new());
        scan_fns(&body, "t.rs", &mut fns);
        scan_source(&body, "t.rs", &mut discovered);

        let adopted = parse_source(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: worker;
                node B = Terminate, task: b, dataflow: [worker];
                node R = Terminate, task: r, reads: [crate::OUT];
            }
        "#,
            "t.rs",
        )
        .unwrap();
        let text = render(
            &adopted[0],
            &Options {
                runtime: true,
                discovered: discovered.clone(),
                ..Default::default()
            },
        );
        assert!(!text.contains("n_A -- \"discovered\""), "{text}");
        assert!(
            text.contains("n_B -- \"discovered\" --> s_crate__OUT"),
            "{text}"
        );
        let coverage = coverage_warnings(&adopted, &fns, &[], &[]);
        assert!(coverage.is_empty(), "{coverage:?}");

        let bare = parse_source(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: worker;
                node R = Terminate, task: r, reads: [crate::OUT];
            }
        "#,
            "t.rs",
        )
        .unwrap();
        let lints = dataflow_lints(&bare, &discovered, &[], &LintCats::all(), &[]);
        assert!(
            lints
                .iter()
                .any(|w| w.contains("orphan read: `crate::OUT` is read by R")),
            "{lints:?}"
        );
        let coverage = coverage_warnings(&bare, &fns, &[], &[]);
        assert!(
            coverage
                .iter()
                .any(|w| w.contains("`t::worker` is not bound")),
            "{coverage:?}"
        );
    }

    #[test]
    fn a_gated_discover_or_adoption_draws_its_predicate() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: worker, #[cfg(feature = "x")] discover;
                node B = Terminate, task: b, dataflow: [#[cfg(feature = "y")] crate::setter];
            }
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode) { node.put(&crate::OUT, 1); }
            #[embassy_supervisor::dataflow]
            fn setter(node: &'static TaskNode) { node.put(&crate::SET, 1); }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let mut opts = Options {
            runtime: true,
            discovered,
            ..Default::default()
        };
        let text = render(&d[0], &opts);
        assert!(
            text.contains(
                "n_A -- \"discovered · <small>cfg(feature=#quot;x#quot;)</small>\" --> s_crate__OUT"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "n_B -- \"discovered · <small>cfg(feature=#quot;y#quot;)</small>\" --> s_crate__SET"
            ),
            "{text}"
        );
        opts.show_cfg = false;
        let hidden = render(&d[0], &opts);
        assert!(!hidden.contains("cfg("), "{hidden}");
    }

    #[test]
    fn a_discovered_edge_keeps_its_shortest_cfg_set() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: worker, #[cfg(feature = "x")] discover,
                    dataflow: [crate::setter];
            }
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode) { node.put(&crate::S, 1); }
            #[embassy_supervisor::dataflow]
            fn setter(node: &'static TaskNode) { node.put(&crate::S, 1); }
        "#;
        let gated_call = src.replace(
            "fn setter(node: &'static TaskNode) { node.put",
            "fn setter(node: &'static TaskNode) { #[cfg(feature = \"y\")] node.put",
        );
        let edge = |src: &str| {
            let d = parse_source(src, "t.rs").unwrap();
            let mut discovered = Vec::new();
            scan_source(&parsed(src), "t.rs", &mut discovered);
            let text = render(
                &d[0],
                &Options {
                    runtime: true,
                    show_cfg: true,
                    discovered,
                    ..Default::default()
                },
            );
            let edges: Vec<String> = text
                .lines()
                .filter(|l| l.trim_start().starts_with("n_W --") && l.ends_with("--> s_crate__S"))
                .map(str::to_string)
                .collect();
            assert_eq!(edges.len(), 1, "one edge per direction and signal: {text}");
            edges[0].clone()
        };
        let ungated = edge(src);
        assert!(
            !ungated.contains("cfg("),
            "the ungated adoption labels the edge the gated task fn also draws: {ungated}"
        );
        let tie = edge(&gated_call);
        assert_eq!(tie.matches("cfg(").count(), 1, "{tie}");
    }

    #[test]
    fn provides_draws_the_provider_into_the_slot() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node NET = Terminate, deps: [], task: net_task, provides: [HANDLE];
                node APP = Terminate, deps: [NET], task: app_task,
                    resources: [HANDLE: shared Handle];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        for opts in [
            Options::default(),
            Options {
                runtime: true,
                ..Default::default()
            },
        ] {
            let text = render(&d[0], &opts);
            assert!(
                text.contains("n_NET -- \"provides\" --> r_HANDLE"),
                "{text}"
            );
        }
    }

    #[test]
    fn entry_markers_draw_beside_the_edge() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f,
                    reads: [crate::IN],
                    writes: [crate::OUT observed beat via it.get()];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                ..Default::default()
            },
        );
        assert!(
            text.contains("s_crate__IN --> n_A"),
            "an unmarked read: {text}"
        );
        assert!(text.contains("\"observed · beat\""), "{text}");
    }

    const FRAGMENT: &str = r#"
        embassy_supervisor::supervisor_fragment! {
            name: NET_FRAG;
            node NET = Terminate, deps: [], task: $crate::net::net_task,
                writes: [$crate::net::STACK],
                resources: [USB: embassy_rp::Peri<'static, embassy_rp::peripherals::USB>];
        }
    "#;

    const COMPOSE: &str = r#"
        embassy_supervisor::compose_graph! {
            fragments: [NET_FRAG, HTTP_FRAG],
            graph: {
                node APP = Terminate, deps: [NET ready], task: crate::app::run,
                    reads: [crate::net::STACK];
            }
        }
    "#;

    #[test]
    fn a_fragment_parses_on_its_own_and_shows_its_dollar_crate() {
        let d = parse_source(FRAGMENT, "net.rs").unwrap();
        assert_eq!(d[0].kind, DeclKind::Fragment);
        assert_eq!(d[0].name().as_deref(), Some("NET_FRAG"));
        let out = render(
            &d[0],
            &Options {
                signals: true,
                full_paths: true,
                ..Default::default()
            },
        );
        assert!(out.contains("$crate::net::STACK"), "{out}");
    }

    #[test]
    fn compose_splices_the_fragments_it_can_find_and_names_the_rest() {
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (out, warnings) = resolve(decls);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DeclKind::Compose);
        assert_eq!(out[0].spec.items.len(), 2);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("HTTP_FRAG"), "{warnings:?}");
    }

    #[test]
    fn an_ambiguous_fragment_name_is_never_spliced() {
        let mut decls = parse_source(FRAGMENT, "crate_a/net.rs").unwrap();
        decls.extend(parse_source(FRAGMENT, "crate_b/net.rs").unwrap());
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (out, warnings) = resolve(decls);
        assert_eq!(out.len(), 3, "compose site + both standalone fragments");
        assert_eq!(
            out.iter().filter(|d| d.kind == DeclKind::Fragment).count(),
            2
        );
        assert!(
            warnings.iter().any(|w| w.contains("ambiguous")
                && w.contains("crate_a/net.rs")
                && w.contains("crate_b/net.rs")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_composed_graph_reads_as_one_graph_by_default() {
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (out, _) = resolve(decls);
        let plain = render(&out[0], &Options::default());
        assert!(!plain.contains("subgraph"), "{plain}");
        assert!(plain.contains("n_NET -- \"ready\" --> n_APP"), "{plain}");

        let grouped = render(
            &out[0],
            &Options {
                fragments: true,
                ..Default::default()
            },
        );
        assert!(
            grouped.contains("subgraph f_NET_FRAG[\"NET_FRAG\"]"),
            "{grouped}"
        );
        assert!(
            grouped.contains("n_NET -- \"ready\" --> n_APP"),
            "{grouped}"
        );
    }

    #[test]
    fn a_fragments_dollar_crate_resolves_to_the_composing_crate() {
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (out, _) = resolve(decls);
        let text = render(
            &out[0],
            &Options {
                signals: true,
                ..Default::default()
            },
        );
        assert_eq!(text.matches("[/\"STACK\"/]").count(), 1, "{text}");
        assert!(text.contains("n_NET -.-> s_crate__net__STACK"), "{text}");
        assert!(text.contains("s_crate__net__STACK -.-> n_APP"), "{text}");
    }

    #[test]
    fn a_fragment_from_another_crate_keeps_that_crates_path() {
        let compose = r#"
            embassy_supervisor::compose_graph! {
                fragments: [::net_stack::NET_FRAG],
                graph: { node APP = Terminate, deps: [NET], task: f; }
            }
        "#;
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(compose, "main.rs").unwrap());
        let (out, _) = resolve(decls);
        let text = render(
            &out[0],
            &Options {
                signals: true,
                full_paths: true,
                ..Default::default()
            },
        );
        assert!(text.contains("::net_stack::net::STACK"), "{text}");
    }

    #[test]
    fn an_unconsumed_fragment_is_still_drawn_on_its_own() {
        let (out, _) = resolve(parse_source(FRAGMENT, "net.rs").unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DeclKind::Fragment);
    }

    #[test]
    fn a_pool_reports_its_member_count_and_bounds() {
        let src = r#"
            embassy_supervisor::supervisor_fragment! {
                name: HTTP_FRAG;
                pool HTTP = [Terminate, OnDemand], deps: [NET ready],
                    task: $crate::http::http_task,
                    policy: embassy_supervisor::DeferredShrink::new(D::from_secs(4)),
                    min: $crate::http::FLOOR, max: $crate::http::CEIL,
                    slot_timeout: 2000;
            }
        "#;
        let d = parse_source(src, "http.rs").unwrap();
        let text = render(&d[0], &Options::default());
        assert!(text.contains("pool ×2"), "{text}");
        assert!(text.contains("FLOOR..CEIL"), "{text}");
        assert!(text.contains("n_HTTP[["), "{text}");
    }

    #[test]
    fn a_one_token_compose_clause_is_an_error_not_a_panic() {
        let src = r#"
            embassy_supervisor::compose_graph! {
                graph
            }
        "#;
        let err = match parse_source(src, "t.rs") {
            Ok(_) => panic!("one-token clause accepted"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("missing its `:` and value"), "{err}");
    }

    #[test]
    fn a_node_and_a_resource_sharing_a_name_stay_two_boxes() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node WD = Terminate, deps: [], task: f,
                    resources: [WD: embassy_rp::watchdog::Watchdog];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let text = render(&d[0], &Options::default());
        assert!(text.contains("n_WD"), "{text}");
        assert!(text.contains("r_WD"), "{text}");
    }

    #[test]
    fn a_macro_rules_relay_is_not_mistaken_for_a_declaration() {
        let src = r#"
            macro_rules! compose_graph {
                (@next [], {$($acc:tt)*}, {$($g:tt)*}) => {
                    $crate::supervisor_graph! { $($acc)* $($g)* }
                };
                (@emit $g:tt) => {
                    $crate::supervisor_graph! { @internal $g }
                };
            }
        "#;
        assert!(parse_source(src, "lib.rs").unwrap().is_empty());
    }

    #[test]
    fn a_feature_gated_construct_parses_without_that_feature() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                observe writes: it.get();
                node A = Terminate, deps: [], task: f, state: Buf = Buf::new(),
                    writes: [crate::S observed beat], beat_timeout: 10, ready_on_write;
                node B = Terminate, deps: [A ready bound], task: g;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let text = render(
            &d[0],
            &Options {
                signals: true,
                ..Default::default()
            },
        );
        assert!(text.contains("ready_on_write"), "{text}");
        assert!(text.contains("\"observed · beat\""), "{text}");
        assert!(text.contains("\"ready bound\""), "{text}");
    }

    const COUPLED: &str = r#"
        embassy_supervisor::supervisor_graph! {
            node NET = Terminate, deps: [], task: f, writes: [crate::STACK];
            node APP = Terminate, deps: [NET ready], task: g,
                reads: [crate::STACK], writes: [crate::OUT observed via it.get()];
            node LONE = Terminate, deps: [APP], task: h;
        }
    "#;

    fn runtime(src: &str) -> String {
        let d = parse_source(src, "t.rs").unwrap();
        render(
            &d[0],
            &Options {
                runtime: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn the_runtime_view_drops_every_bring_up_edge() {
        let out = runtime(COUPLED);
        let node_to_node: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("n_") && l.contains("-->") || l.contains(".->"))
            .filter(|l| {
                l.split_once("->")
                    .is_some_and(|(_, rhs)| rhs.trim_start_matches('>').trim().starts_with("n_"))
                    && l.starts_with("n_")
            })
            .collect();
        assert!(node_to_node.is_empty(), "{node_to_node:?} in:\n{out}");
        assert!(!out.contains("\"ready\""), "{out}");
        assert!(out.contains("runtime coupling"), "{out}");
    }

    #[test]
    fn the_runtime_view_draws_every_signal_without_asking() {
        let out = runtime(COUPLED);
        assert!(out.contains("[/\"STACK\"/]"), "{out}");
        assert!(out.contains("[/\"OUT\"/]"), "{out}");
        assert!(out.contains("n_NET --> s_crate__STACK"), "{out}");
        assert!(out.contains("s_crate__STACK --> n_APP"), "{out}");
        assert!(
            out.contains("n_APP -- \"observed\" --> s_crate__OUT"),
            "{out}"
        );
    }

    #[test]
    fn signal_labels_grow_until_they_name_one_signal() {
        let out = runtime(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f,
                    writes: [crate::rate::params::TABLE, crate::angle::params::TABLE,
                             crate::net::STACK];
            }
        "#,
        );
        assert!(out.contains("[/\"rate::params::TABLE\"/]"), "{out}");
        assert!(out.contains("[/\"angle::params::TABLE\"/]"), "{out}");
        assert!(out.contains("[/\"STACK\"/]"), "{out}");
    }

    #[test]
    fn a_node_declaring_no_dataflow_is_still_drawn() {
        let out = runtime(COUPLED);
        assert!(out.contains("n_LONE["), "{out}");
        assert!(!out.contains("n_LONE -"), "{out}");
    }

    #[test]
    fn the_two_views_disagree_about_the_same_graph() {
        let d = parse_source(COUPLED, "t.rs").unwrap();
        let bringup = render(&d[0], &Options::default());
        assert!(
            bringup.contains("n_NET -- \"ready\" --> n_APP"),
            "{bringup}"
        );
        assert!(!bringup.contains("STACK"), "{bringup}");
    }

    const RESOURCED: &str = r#"
        embassy_supervisor::supervisor_graph! {
            node NET = Terminate, deps: [], task: f,
                resources: [USB: Peri<USB>, STACK_H: shared Stack],
                writes: [crate::STACK];
            node HTTP = Terminate, deps: [NET ready], task: g,
                resources: [STACK_H: shared Stack, BUF: consume Buf];
            node WD = Terminate, deps: [], task: h, resources: [WD_DEV: Watchdog];
            node LONE = Terminate, deps: [NET], task: k;
            node PWR = Terminate, deps: [], task: m,
                resources: [BUDGET: divisible, BUS: shared serialized Bus];
            node PWR2 = Terminate, deps: [], task: m,
                resources: [BUDGET: divisible, BUS: shared serialized Bus];
        }
    "#;

    fn anchored(src: &str) -> String {
        let d = parse_source(src, "t.rs").unwrap();
        render(
            &d[0],
            &Options {
                runtime: true,
                anchor_uncoupled: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_resource_slot_flows_into_the_node_that_takes_it() {
        let out = runtime(RESOURCED);
        assert!(
            out.contains("r_USB@{ shape: notch-rect, label: \"USB\" }"),
            "{out}"
        );
        assert!(out.contains("r_USB --> n_NET"), "{out}");
        assert!(out.contains("r_BUF -- \"consume\" --> n_HTTP"), "{out}");
    }

    #[test]
    fn a_veto_write_carries_its_mark() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node OC = Terminate, deps: [], task: f, writes: [crate::TRIP veto];
                node DIFF = Terminate, deps: [], task: f, writes: [crate::TRIP veto observed beat];
                node BREAKER = Terminate, deps: [], task: g, reads: [crate::TRIP];
            }
        "#;
        let out = runtime(src);
        assert!(out.contains("n_OC -- \"veto\" --> s_crate__TRIP"), "{out}");
        assert!(
            out.contains("n_DIFF -- \"observed · beat · veto\" --> s_crate__TRIP"),
            "{out}"
        );
        let decls = parse_source(src, "t.rs").unwrap();
        let full = full_model(&decls);
        let model::ItemModel::Node(oc) = &full.graphs[0].items[0] else {
            panic!("node")
        };
        assert!(oc.writes[0].veto);
        let j = &model_json(&decls, &[])["graphs"][0]["items"][0]["writes"][0];
        assert_eq!(j["veto"], true);
    }

    #[test]
    fn a_divisible_budget_and_a_serialized_bus_carry_their_marks() {
        let out = runtime(RESOURCED);
        assert_eq!(out.matches("label: \"BUDGET\"").count(), 1, "{out}");
        assert!(out.contains("r_BUDGET -- \"divisible\" --> n_PWR"), "{out}");
        assert!(
            out.contains("r_BUDGET -- \"divisible\" --> n_PWR2"),
            "{out}"
        );
        assert!(
            out.contains("r_BUS -- \"shared · serialized\" --> n_PWR"),
            "{out}"
        );
        let decls = parse_source(RESOURCED, "t.rs").unwrap();
        let full = full_model(&decls);
        let model::ItemModel::Node(pwr) = &full.graphs[0].items[4] else {
            panic!("node")
        };
        assert_eq!(pwr.name, "PWR");
        assert!(pwr.resources[0].divisible && !pwr.resources[0].serialized);
        assert!(pwr.resources[1].serialized && pwr.resources[1].shared);
        let j = &model_json(&decls, &[])["graphs"][0]["items"][4]["resources"];
        assert_eq!(j[0]["divisible"], true);
        assert_eq!(j[0]["serialized"], false);
        assert_eq!(j[1]["serialized"], true);
    }

    #[test]
    fn a_shared_slot_is_one_box_with_the_fan_out_visible() {
        let out = runtime(RESOURCED);
        assert_eq!(out.matches("label: \"STACK_H\"").count(), 1, "{out}");
        assert!(out.contains("r_STACK_H -- \"shared\" --> n_NET"), "{out}");
        assert!(out.contains("r_STACK_H -- \"shared\" --> n_HTTP"), "{out}");
    }

    #[test]
    fn a_slot_is_drawn_in_both_views_because_it_is_both_things() {
        let d = parse_source(RESOURCED, "t.rs").unwrap();
        let bringup = render(&d[0], &Options::default());
        assert!(bringup.contains("r_USB --> n_NET"), "{bringup}");
        assert!(!bringup.contains("STACK["), "{bringup}");
    }

    #[test]
    fn a_node_holding_only_a_resource_is_not_floating() {
        let out = anchored(RESOURCED);
        assert!(out.contains("r_WD_DEV --> n_WD"), "{out}");
        assert!(
            !out.contains("n_WD -.") && !out.contains(".-> n_WD"),
            "{out}"
        );
    }

    #[test]
    fn anchoring_pins_a_node_that_has_neither_signals_nor_resources() {
        let out = anchored(RESOURCED);
        assert!(out.contains("n_NET -. \"spawn\" .-> n_LONE"), "{out}");
        assert_eq!(out.matches("spawn").count(), 1, "{out}");
    }

    #[test]
    fn anchoring_works_from_either_end_of_the_edge() {
        let out = anchored(
            r#"
            embassy_supervisor::supervisor_graph! {
                node ROOT = Terminate, deps: [], task: f;
                node USER = Terminate, deps: [ROOT ready], task: g,
                    reads: [crate::S];
            }
        "#,
        );
        assert!(
            out.contains("n_ROOT -. \"spawn · ready\" .-> n_USER"),
            "{out}"
        );
    }

    #[test]
    fn anchoring_leaves_edges_between_coupled_nodes_out() {
        let out = anchored(
            r#"
            embassy_supervisor::supervisor_graph! {
                node ROOT = Terminate, deps: [], task: root;
                node MID = Terminate, deps: [ROOT], task: mid, writes: [crate::S];
                node LEAF = Terminate, deps: [MID], task: leaf, reads: [crate::S];
            }
        "#,
        );
        assert!(out.contains("n_ROOT -. \"spawn\" .-> n_MID"), "{out}");
        assert!(
            !out.contains("n_MID -. \"spawn\" .-> n_LEAF"),
            "the coupled-to-coupled edge remains out of the runtime view: {out}"
        );
    }

    #[test]
    fn runtime_deps_restores_every_bring_up_edge_as_spawn_context() {
        let d = parse_source(COUPLED, "t.rs").unwrap();
        let out = render(
            &d[0],
            &Options {
                runtime: true,
                runtime_deps: true,
                ..Default::default()
            },
        );
        assert!(
            out.contains("n_NET -. \"spawn · ready\" .-> n_APP"),
            "the coupled-to-coupled dependency returns: {out}"
        );
        assert!(out.contains("n_APP -. \"spawn\" .-> n_LONE"), "{out}");
    }

    #[test]
    fn anchoring_counts_discovered_and_adopted_accesses_as_coupling() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node ROOT = Terminate, deps: [], task: root, writes: [crate::ROOT_SIGNAL];
                node DISCOVERED = Terminate, deps: [ROOT], task: discovered, discover;
                node ADOPTED = Terminate, deps: [ROOT], dataflow: [crate::cfg::set_value];
            }
            #[embassy_supervisor::dataflow]
            async fn discovered(node: &'static TaskNode) {
                let _ = node.get(&crate::DISCOVERED_SIGNAL);
            }
            mod cfg {
                #[embassy_supervisor::dataflow]
                pub fn set_value(node: &'static TaskNode, value: u32) {
                    node.put(&crate::ADOPTED_SIGNAL, value);
                }
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let out = render(
            &d[0],
            &Options {
                runtime: true,
                anchor_uncoupled: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(
            out.contains("s_crate__DISCOVERED_SIGNAL -- \"discovered\" --> n_DISCOVERED"),
            "{out}"
        );
        assert!(
            out.contains("n_ADOPTED -- \"discovered\" --> s_crate__ADOPTED_SIGNAL"),
            "{out}"
        );
        assert!(
            !out.contains("n_ROOT -. \"spawn\" .-> n_DISCOVERED"),
            "the discovered reader has runtime coupling: {out}"
        );
        assert!(
            !out.contains("n_ROOT -. \"spawn\" .-> n_ADOPTED"),
            "the adopted writer has runtime coupling: {out}"
        );
    }

    #[test]
    fn anchoring_counts_a_drawn_provides_edge_as_coupling() {
        let taken = anchored(
            "embassy_supervisor::supervisor_graph! { \
             node NET = Terminate, deps: [], task: n, provides: [STACK]; \
             node APP = Terminate, deps: [NET], task: a, resources: [STACK: Stack]; }",
        );
        assert!(
            taken.contains("n_NET -- \"provides\" --> r_STACK"),
            "{taken}"
        );
        assert!(!taken.contains("spawn"), "{taken}");
        let untaken = anchored(
            "embassy_supervisor::supervisor_graph! { \
             node NET = Terminate, deps: [], task: n, provides: [STACK]; \
             node APP = Terminate, deps: [NET], task: a; }",
        );
        assert!(
            untaken.contains("n_NET -. \"spawn\" .-> n_APP"),
            "{untaken}"
        );
    }

    #[test]
    fn without_anchoring_the_runtime_view_keeps_no_bring_up_edge() {
        assert!(!runtime(RESOURCED).contains("spawn"));
    }

    const FOUR: &str = r#"
        embassy_supervisor::supervisor_graph! {
            node NET = Terminate, deps: [], task: f,
                resources: [USB: Peri], writes: [crate::STACK];
            node APP = Terminate, deps: [NET ready], task: g, reads: [crate::STACK];
            node LOG = Terminate, deps: [NET], task: h, writes: [crate::LINES];
            pool WORK = [Terminate], deps: [NET], task: w,
                policy: P::new(), min: 1, max: 2, reads: [crate::STACK];
        }
    "#;

    fn without(src: &str, names: &[&str], f: impl FnOnce(&mut Options)) -> String {
        let mut d = parse_source(src, "t.rs").unwrap();
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        assert!(exclude(&mut d[0], &names).is_empty(), "unmatched name");
        let mut o = Options::default();
        f(&mut o);
        render(&d[0], &o)
    }

    #[test]
    fn an_excluded_node_takes_its_edges_with_it() {
        let out = without(FOUR, &["LOG"], |_| {});
        assert!(!out.contains("n_LOG"), "{out}");
        assert!(!out.contains("--> n_LOG"), "{out}");
        assert!(out.contains("n_NET -- \"ready\" --> n_APP"), "{out}");
    }

    #[test]
    fn what_only_the_excluded_node_used_goes_with_it() {
        let out = without(FOUR, &["LOG"], |o| o.runtime = true);
        assert!(!out.contains("LINES"), "{out}");
        assert!(out.contains("[/\"STACK\"/]"), "{out}");
    }

    #[test]
    fn a_signal_someone_still_reads_survives_losing_its_writer() {
        let out = without(FOUR, &["NET"], |o| o.runtime = true);
        assert!(out.contains("[/\"STACK\"/]"), "{out}");
        assert!(out.contains("s_crate__STACK --> n_APP"), "{out}");
        assert!(!out.contains("USB"), "{out}");
    }

    #[test]
    fn a_pool_can_be_excluded_by_name() {
        let out = without(FOUR, &["WORK"], |_| {});
        assert!(!out.contains("n_WORK"), "{out}");
    }

    #[test]
    fn the_states_view_is_filtered_the_same_way() {
        let out = without(FOUR, &["APP", "LOG"], |o| o.states = true);
        assert!(!out.contains("APP") && !out.contains("LOG"), "{out}");
        assert!(out.contains("NET"), "{out}");
    }

    #[test]
    fn a_name_matching_nothing_is_reported_rather_than_ignored() {
        let mut d = parse_source(FOUR, "t.rs").unwrap();
        let names = vec!["NET".to_string(), "NOPE".to_string()];
        assert_eq!(exclude(&mut d[0], &names), vec!["NOPE".to_string()]);
    }

    fn legended(src: &str, f: impl FnOnce(&mut Options)) -> String {
        let mut o = Options {
            legend: true,
            ..Default::default()
        };
        f(&mut o);
        let d = parse_source(src, "t.rs").unwrap();
        render(&d[0], &o)
    }

    const TWO: &str = "embassy_supervisor::supervisor_graph! { \
        node A = Terminate, deps: [], task: f; node B = Terminate, deps: [A], task: g; }";

    #[test]
    fn the_graph_and_the_legend_are_two_chained_subgraphs() {
        let out = legended(TWO, |_| {});
        assert!(out.contains("subgraph __sv_graph[\" \"]"), "{out}");
        assert!(out.contains("subgraph legend[\"legend\"]"), "{out}");
        assert!(out.contains("__sv_graph ~~~ legend"), "{out}");
        assert!(
            out.contains("style __sv_graph fill:none,stroke:none"),
            "{out}"
        );
    }

    #[test]
    fn the_wrapper_carries_the_direction_it_would_otherwise_rotate() {
        let out = legended(TWO, |_| {});
        let wrapper = out.split("subgraph __sv_graph").nth(1).expect("wrapper");
        assert!(wrapper.starts_with("[\" \"]\n    direction TD"), "{out}");

        let d = parse_source(TWO, "t.rs").unwrap();
        let without = render(&d[0], &Options::default());
        assert!(!without.contains("__sv_graph"), "no legend, no wrapper");
    }

    #[test]
    fn the_legend_can_be_a_diagram_of_its_own() {
        let apart = legend_diagram(&Options::default());
        assert!(apart.contains("%% legend\nflowchart LR"), "{apart}");
        assert!(apart.contains("subgraph legend[\"legend\"]"), "{apart}");
        assert!(!apart.contains("~~~"), "{apart}");
        assert!(apart.contains("classDef signal"), "{apart}");
    }

    #[test]
    fn the_separate_legend_matches_the_view_it_explains() {
        let bringup = legend_diagram(&Options::default());
        assert!(
            bringup.contains("[\"writer\"] -.-> lg3b[/\"signal\"/] -.-> lg3c[\"reader\"]"),
            "{bringup}"
        );
        assert!(bringup.contains("signal found in the body"), "{bringup}");

        let runtime = legend_diagram(&Options {
            runtime: true,
            ..Default::default()
        });
        assert!(runtime.contains("polled by the supervisor"), "{runtime}");
        assert!(!runtime.contains("readiness propagates"), "{runtime}");

        let states = legend_diagram(&Options {
            states: true,
            ..Default::default()
        });
        assert!(states.contains("%% legend\nstateDiagram-v2"), "{states}");
        assert!(
            states.contains("an observation, and it stays put"),
            "{states}"
        );
    }

    #[test]
    fn a_legend_tightens_the_row_spacing_and_only_then() {
        let with = legended(TWO, |_| {});
        assert!(
            with.contains("---\n%%{init:"),
            "the init line follows the frontmatter: {with}"
        );
        assert!(with.contains("\"nodeSpacing\": 12"), "{with}");

        let d = parse_source(TWO, "t.rs").unwrap();
        let without = render(&d[0], &Options::default());
        assert!(!without.contains("init:"), "{without}");
        assert!(without.contains("---\n%% supervisor_graph!"), "{without}");
    }

    #[test]
    fn the_legend_comes_last_in_the_output() {
        let out = legended(TWO, |_| {});
        let legend_at = out.find("subgraph legend").expect("legend");
        assert!(
            legend_at > out.rfind("classDef parked").expect("classdefs"),
            "{out}"
        );
    }

    #[test]
    fn the_runtime_legend_explains_the_runtime_edges() {
        let out = legended(TWO, |o| {
            o.runtime = true;
            o.anchor_uncoupled = true;
        });
        assert!(out.contains("polled by the supervisor"), "{out}");
        assert!(out.contains("pinned by bring-up order"), "{out}");
        assert!(!out.contains("readiness propagates"), "{out}");

        let all_deps = legended(TWO, |o| {
            o.runtime = true;
            o.runtime_deps = true;
            o.anchor_uncoupled = true;
        });
        assert!(
            all_deps.contains("bring-up order shown as context"),
            "{all_deps}"
        );
        assert!(
            !all_deps.contains("pinned by bring-up order"),
            "the full mode supersedes the anchor-only legend: {all_deps}"
        );
    }

    #[test]
    fn no_state_label_contains_a_semicolon() {
        let src = "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], \
                   task: f, beat_timeout: 10, cancel; }";
        let out = legended(src, |o| o.states = true);
        for line in out.lines() {
            if let Some((_, label)) = line.split_once(": ") {
                assert!(!label.contains(';'), "label ends early: {line}");
            }
        }
        assert!(
            legend_diagram(&Options {
                states: true,
                ..Default::default()
            })
            .lines()
            .all(|l| l.split_once(": ").is_none_or(|(_, r)| !r.contains(';')))
        );
    }

    #[test]
    fn the_states_view_has_a_legend_of_its_own() {
        let out = legended(TWO, |o| o.states = true);
        assert!(out.contains("as legend {"), "{out}");
        assert!(out.contains("an observation, and it stays put"), "{out}");
    }

    #[test]
    fn the_legend_is_set_in_smaller_type_than_the_graph() {
        let out = legended(TWO, |_| {});
        assert!(out.contains("classDef legendtext font-size:11px;"), "{out}");
        assert!(
            regex_like(&out, "linkStyle ", " font-size:10px;"),
            "no edge-label sizing:\n{out}"
        );
    }

    #[test]
    fn the_legend_styles_its_own_links_after_an_open_link() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w, writes: [crate::sig::ARR[0]];
                node R = Terminate, task: r, reads: [crate::sig::ARR];
            }
        "#;
        let out = legended(src, |o| o.runtime = true);
        assert!(
            out.contains("  linkStyle 3,4,5,6,7,8,9,10,11,12,13 font-size:10px;"),
            "graph links 0-2, then the legend's 11: {out}"
        );
    }

    fn regex_like(s: &str, head: &str, tail: &str) -> bool {
        s.split(head).skip(1).any(|rest| {
            rest.lines()
                .next()
                .is_some_and(|line| line.ends_with(tail.trim_end()))
        })
    }

    fn spaced(h: Option<u32>, v: Option<u32>, f: impl FnOnce(&mut Options)) -> String {
        let mut o = Options {
            h_spacing: h,
            v_spacing: v,
            ..Default::default()
        };
        f(&mut o);
        let d = parse_source(TWO, "t.rs").unwrap();
        render(&d[0], &o)
    }

    #[test]
    fn the_axes_are_named_for_the_page_and_mapped_to_the_flow() {
        let td = spaced(Some(20), Some(90), |_| {});
        assert!(
            td.contains(
                "---\n%%{init: {\"flowchart\": {\"nodeSpacing\": 20, \"rankSpacing\": 90}} }%%"
            ),
            "{td}"
        );

        let lr = spaced(Some(20), Some(90), |o| o.direction = "LR".to_string());
        assert!(
            lr.contains(
                "---\n%%{init: {\"flowchart\": {\"nodeSpacing\": 90, \"rankSpacing\": 20}} }%%"
            ),
            "{lr}"
        );
    }

    #[test]
    fn only_what_was_asked_for_is_set() {
        assert!(spaced(None, None, |_| {}).contains("---\n%% supervisor_graph!"));
        let h = spaced(Some(30), None, |_| {});
        assert!(h.contains("\"nodeSpacing\": 30"), "{h}");
        assert!(!h.contains("rankSpacing"), "{h}");
    }

    #[test]
    fn an_explicit_spacing_beats_the_legends_default() {
        assert!(spaced(None, None, |o| o.legend = true).contains("\"nodeSpacing\": 12"));
        assert!(spaced(Some(40), None, |o| o.legend = true).contains("\"nodeSpacing\": 40"));
    }

    #[test]
    fn a_state_diagram_takes_the_same_setting() {
        let out = spaced(None, Some(80), |o| o.states = true);
        assert!(
            out.contains("%%{init: {\"state\": {\"rankSpacing\": 80}} }%%"),
            "{out}"
        );
        assert!(!out.contains("\"flowchart\""), "{out}");
        assert!(out.contains("stateDiagram-v2"), "{out}");
        let legend = spaced(None, None, |o| {
            o.states = true;
            o.legend = true;
        });
        assert!(!legend.contains("nodeSpacing"), "{legend}");
    }

    #[test]
    fn every_container_gets_the_direction_not_just_the_diagram() {
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (out, _) = resolve(decls);
        let text = render(
            &out[0],
            &Options {
                direction: "LR".to_string(),
                fragments: true,
                legend: true,
                ..Default::default()
            },
        );
        assert!(text.contains("flowchart LR"), "{text}");
        assert!(
            text.contains("subgraph f_NET_FRAG[\"NET_FRAG\"]\n    direction LR"),
            "{text}"
        );
        assert!(
            text.contains("subgraph legend[\"legend\"]\n    direction LR"),
            "{text}"
        );
    }

    #[test]
    fn a_state_composite_carries_the_direction_too() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node NET = Terminate, deps: [], task: f;
                node APP = Terminate, deps: [NET ready], task: g;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let out = render(
            &d[0],
            &Options {
                direction: "LR".to_string(),
                states: true,
                ..Default::default()
            },
        );
        assert_eq!(out.matches("direction LR").count(), 4, "{out}");
    }

    #[test]
    fn the_td_translation_reaches_the_composites_as_well() {
        let out = states("embassy_supervisor::supervisor_graph! { node A = Terminate, deps: []; }");
        assert!(!out.contains("direction TD"), "{out}");
        assert_eq!(out.matches("direction TB").count(), 2, "{out}");
    }

    fn states(src: &str) -> String {
        let d = parse_source(src, "t.rs").unwrap();
        render(
            &d[0],
            &Options {
                states: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn each_mode_gets_the_lifecycle_the_crate_documents_for_it() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f;
                node B = Pause, deps: [], task: g;
            }
        "#,
        );
        assert!(out.starts_with("---\ntitle:"), "{out}");
        assert!(out.contains("stateDiagram-v2"), "{out}");
        assert!(!out.contains("accTitle:"), "{out}");
        assert!(out.contains("accDescr:"), "{out}");
        assert!(out.contains("respawn_terminate"), "{out}");
        assert!(out.contains("parks on wait_resume()"), "{out}");
        assert!(out.contains("resume_node or resume_pausable"), "{out}");
        assert!(
            !out.contains("g0_run --> g1_run"),
            "composites stay separate"
        );
    }

    #[test]
    fn the_flowcharts_default_direction_is_translated() {
        let out = states("embassy_supervisor::supervisor_graph! { node A = Terminate, deps: []; }");
        assert!(out.contains("direction TB"), "{out}");
        assert!(!out.contains("direction TD"), "{out}");
    }

    #[test]
    fn nodes_sharing_a_lifecycle_share_one_diagram() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f;
                node B = Terminate, deps: [], task: g;
                node C = Terminate, deps: [], task: h, cancel;
            }
        "#,
        );
        assert!(out.contains("Terminate — A, B"), "{out}");
        assert!(out.contains("Terminate · cancel — C"), "{out}");
        assert_eq!(out.matches("stateDiagram-v2").count(), 1);
    }

    #[test]
    fn cancel_changes_what_a_stop_does_to_the_worker() {
        let plain = states(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], task: f; }",
        );
        let cancelled = states(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], task: f, cancel; }",
        );
        assert!(plain.contains("acks, exits its loop"), "{plain}");
        assert!(cancelled.contains("future dropped in place"), "{cancelled}");
    }

    #[test]
    fn readiness_is_a_state_of_running_not_a_state_beside_it() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node NET = Terminate, deps: [], task: f;
                node APP = Terminate, deps: [NET ready], task: g;
            }
        "#,
        );
        assert!(out.contains("gates dependents — NET"), "{out}");
        assert!(out.contains("state \"running\" as g1_run {"), "{out}");
        assert!(
            out.contains("g1_starting --> g1_ready: the task calls set_ready()"),
            "{out}"
        );
        assert!(out.contains("Terminate — APP"), "{out}");
    }

    #[test]
    fn ready_on_write_says_who_asserts_readiness() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f, beat_timeout: 10, ready_on_write,
                    writes: [crate::S observed beat via it.get()];
                node B = Terminate, deps: [A ready], task: g;
            }
        "#,
        );
        assert!(
            out.contains("the sweep sees a declared write advance"),
            "{out}"
        );
        assert!(!out.contains("the task calls set_ready()"), "{out}");
    }

    #[test]
    fn only_transitions_a_declaration_implies_are_drawn() {
        let plain = states(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], task: f; }",
        );
        assert!(!plain.contains("activate"), "{plain}");
        assert!(!plain.contains("clear_ready"), "{plain}");

        let bound = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node P = Terminate, deps: [], task: f;
                node C = Terminate, deps: [P ready bound], task: g;
            }
        "#,
        );
        assert!(
            bound.contains("a bound dep called clear_ready()"),
            "{bound}"
        );
    }

    #[test]
    fn a_declared_disabled_node_starts_latched() {
        let out = states(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], task: f, disabled; }",
        );
        assert!(out.contains("[*] --> g0_off"), "{out}");
        assert!(
            out.contains("activate · the one path that clears the latch"),
            "{out}"
        );
    }

    #[test]
    fn a_parked_node_says_who_spawns_it() {
        let out = states("embassy_supervisor::supervisor_graph! { node A = Terminate, deps: []; }");
        assert!(out.contains("the application spawns the task"), "{out}");
    }

    #[test]
    fn a_pool_splits_by_member_mode() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                pool P = [Terminate, OnDemand, OnDemand], deps: [], task: f,
                    policy: Pol::new(), min: 1, max: 3;
            }
        "#,
        );
        assert!(
            out.contains("Terminate · pool member — P member 0"),
            "{out}"
        );
        assert!(
            out.contains("OnDemand · pool member — P members 1..=2"),
            "{out}"
        );
        assert!(out.contains("the pool grows under load"), "{out}");
    }

    #[test]
    fn a_start_predicate_and_a_restart_budget_reach_the_lifecycle() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, deps: [], task: f, enabled_if: crate::on,
                    restart_budget: 3 in 60000;
                node B = Terminate, deps: [], task: g;
            }
        "#,
        );
        assert!(
            out.contains("Terminate · enabled_if · restarts 3/60000ms — A"),
            "{out}"
        );
        assert!(out.contains("Terminate — B"), "{out}");
        assert!(
            regex_like(
                &out,
                "_down --> ",
                "_off: start · the enabled_if predicate returned false"
            ),
            "{out}"
        );
        assert!(
            regex_like(
                &out,
                "_off --> ",
                "activate · the one path that clears the latch"
            ),
            "{out}"
        );
        assert!(!regex_like(&out, "[*] --> ", "_off"), "{out}");
    }

    #[test]
    fn a_bound_on_demand_member_waits_for_the_pool_policy() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                node P = Terminate, deps: [], task: f;
                pool W = [OnDemand], deps: [P ready bound], task: w,
                    policy: Pol::new(), min: 0, max: 1;
                node N = OnDemand, deps: [P ready bound], task: n;
            }
        "#,
        );
        assert!(
            out.contains("that dep is ready again · regrow-eligible, the pool policy restarts it"),
            "{out}"
        );
        assert!(
            out.contains("that dep is ready again · eligible again, start_node brings it up"),
            "{out}"
        );
        assert!(!out.contains("brought back through the gates"), "{out}");
    }

    #[test]
    fn interleaved_member_modes_list_their_members() {
        let out = states(
            r#"
            embassy_supervisor::supervisor_graph! {
                pool P = [Terminate, OnDemand, Terminate], deps: [], task: f,
                    policy: Pol::new(), min: 1, max: 3;
            }
        "#,
        );
        assert!(
            out.contains("Terminate · pool member — P members 0 and 2"),
            "{out}"
        );
        assert!(out.contains("OnDemand · pool member — P member 1"), "{out}");
    }

    #[test]
    fn staleness_leaves_the_node_running() {
        let out = states(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, deps: [], task: f, beat_timeout: 500; }",
        );
        assert!(
            out.contains("g0_run --> g0_run: no beat for 500 ms · reported stale, still running"),
            "{out}"
        );
    }

    #[test]
    fn a_generic_argument_list_is_not_split_on_its_comma() {
        let src = r#"
            embassy_supervisor::compose_graph! {
                fragments: [],
                graph: { node A = Terminate, deps: [], task: f, state: Map<K, V> = m(), cancel; }
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        match &d[0].spec.items[0] {
            Item::Node(n) => assert!(n.cancel, "the clause after the generic was still read"),
            _ => panic!("expected a node"),
        }
    }

    #[test]
    fn the_frontmatter_names_the_diagram_and_the_layout_engine() {
        let g = graph();
        let plain = render(&g, &Options::default());
        assert!(
            plain.starts_with("---\ntitle: supervisor_graph!"),
            "{plain}"
        );
        assert!(!plain.contains("layout:"), "{plain}");
        assert!(!plain.contains("  accTitle: "), "{plain}");
        assert!(plain.contains("  accDescr: "), "{plain}");

        let elk = render(
            &g,
            &Options {
                layout: Some("elk".to_string()),
                ..Default::default()
            },
        );
        assert!(elk.contains("config:\n  layout: elk\n---\n"), "{elk}");

        let custom = render(
            &g,
            &Options {
                title: Some("bring-up: app".to_string()),
                ..Default::default()
            },
        );
        assert!(
            custom.starts_with("---\ntitle: \"bring-up: app\"\n---\n"),
            "{custom}"
        );
        assert!(!custom.contains("accTitle:"), "{custom}");

        let untitled = render(
            &g,
            &Options {
                show_title: false,
                ..Default::default()
            },
        );
        assert!(!untitled.starts_with("---"), "{untitled}");
        assert!(!untitled.contains("\ntitle:"), "{untitled}");
        assert!(!untitled.contains("accTitle:"), "{untitled}");

        let untitled_elk = render(
            &g,
            &Options {
                layout: Some("elk".to_string()),
                show_title: false,
                ..Default::default()
            },
        );
        assert!(
            untitled_elk.starts_with("---\nconfig:\n  layout: elk\n---\n"),
            "{untitled_elk}"
        );
        assert!(!untitled_elk.contains("\ntitle:"), "{untitled_elk}");
    }

    #[test]
    fn a_cfg_predicate_is_label_text_not_a_style() {
        let out = render(&graph(), &Options::default());
        assert!(
            out.contains(
                "EXTRA<br/>Terminate · parked · <small>cfg(feature = #quot;extra#quot;)</small>"
            ),
            "the node draws like any other, predicate small in the label: {out}"
        );
        assert!(!out.contains("conditional"), "{out}");
    }

    #[test]
    fn a_cfg_gated_dep_edge_carries_the_predicate() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: f;
                node B = Terminate, deps: [#[cfg(feature = "x")] A ready], task: g;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let out = render(&d[0], &Options::default());
        assert!(
            out.contains("ready · <small>cfg(feature = #quot;x#quot;)</small>"),
            "{out}"
        );
    }

    #[test]
    fn start_conditions_and_restart_budgets_reach_the_model_json_and_diagram() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: f,
                    enabled_if: crate::settings::radio_on,
                    #[cfg(feature = "x")] restart_budget: 3 in 60000;
                node B = Terminate, task: g;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();

        let full = full_model(&d);
        let model::ItemModel::Node(a) = &full.graphs[0].items[0] else {
            panic!("node")
        };
        let e = a.enabled_if.as_ref().unwrap();
        assert_eq!(
            e.predicate.text.replace(' ', ""),
            "crate::settings::radio_on"
        );
        assert!(e.cfg.is_empty());
        let b = a.restart_budget.as_ref().unwrap();
        assert_eq!(
            (b.max.as_u64(), b.window_ms.as_u64()),
            (Some(3), Some(60000))
        );
        assert_eq!(b.cfg, ["feature=\"x\""]);

        let j = model_json(&d, &[]);
        let a = &j["graphs"][0]["items"][0];
        assert_eq!(
            a["enabled_if"].as_str().unwrap().replace(' ', ""),
            "crate::settings::radio_on"
        );
        assert_eq!(a["restart_budget"]["max"], 3);
        assert_eq!(a["restart_budget"]["window_ms"], 60000);
        assert_eq!(a["restart_budget_cfg"][0], "feature=\"x\"");
        let b = &j["graphs"][0]["items"][1];
        assert!(b["enabled_if"].is_null() && b["restart_budget"].is_null());

        let out = render(&d[0], &Options::default());
        assert!(out.contains("if radio_on"), "{out}");
        assert!(
            out.contains("restarts 3/60000ms <small>cfg(feature = #quot;x#quot;)</small>"),
            "{out}"
        );
    }

    #[test]
    fn a_predicate_fact_keeps_what_is_not_a_plain_path() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: f,
                    enabled_if: || crate::cfg::radio_on() && !crate::cfg::low_battery();
                node B = Terminate, task: g, enabled_if: crate::cfg::flags::<Radio>;
                node C = Terminate, task: h, enabled_if: crate::cfg::radio_on;
                pool P = [Terminate], task: p, policy: Pol::new(), min: 1,
                    max: crate::cfg::cap::<4>();
            }
        "#;
        let out = render(&parse_source(src, "t.rs").unwrap()[0], &Options::default());
        for fact in [
            "if ||crate::cfg::radio_on()#amp;#amp;!crate::cfg::low_battery()",
            "if flags::#lt;Radio#gt;",
            "· if radio_on\"]",
            "1..crate::cfg::cap::#lt;4#gt;()",
        ] {
            assert!(out.contains(fact), "{fact}: {out}");
        }
    }

    #[test]
    fn gated_clause_facts_and_provides_edges_carry_the_predicate() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: f,
                    writes: [crate::S observed beat via it.get()],
                    #[cfg(feature = "x")] beat_timeout: 100,
                    #[cfg(feature = "x")] ready_on_write,
                    provides: [#[cfg(feature = "x")] R];
                node B = Terminate, deps: [A], task: g, resources: [R: u32],
                    #[cfg(feature = "x")] disabled;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let out = render(&d[0], &Options::default());
        let note = "<small>cfg(feature = #quot;x#quot;)</small>";
        assert!(out.contains(&format!("beat 100 {note}")), "{out}");
        assert!(out.contains(&format!("ready_on_write {note}")), "{out}");
        assert!(out.contains(&format!("disabled {note}")), "{out}");
        assert!(out.contains(&format!("provides · {note}")), "{out}");
        let hidden = render(
            &d[0],
            &Options {
                show_cfg: false,
                ..Default::default()
            },
        );
        assert!(!hidden.contains("cfg("), "{hidden}");
    }

    #[test]
    fn cfg_predicates_can_be_hidden() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                #[cfg(feature = "node")]
                node A = Terminate, task: f;
                node B = Terminate, deps: [#[cfg(feature = "dep")] A ready], task: g;
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let visible = render(&d[0], &Options::default());
        let hidden = render(
            &d[0],
            &Options {
                show_cfg: false,
                ..Default::default()
            },
        );
        assert!(visible.contains("cfg("), "{visible}");
        assert!(!hidden.contains("cfg("), "{hidden}");
    }

    #[test]
    fn executor_grouping_boxes_nodes_by_where_they_spawn() {
        let out = render(
            &graph(),
            &Options {
                executors: true,
                ..Default::default()
            },
        );
        let high = out.find("[\"@HIGH\"]").expect("the @HIGH box");
        let end = out[high..].find("\n  end").unwrap() + high;
        assert!(out[high..end].contains("n_HEARTBEAT"), "{out}");
        assert!(
            out.contains("[\"supervisor's executor\"]"),
            "nodes with no executor share the supervisor's executor box: {out}"
        );
        assert!(!out[high..end].contains("n_WATCHDOG"), "{out}");
    }

    #[test]
    fn default_executor_nodes_box_under_its_slot() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                default executor THREAD;
                executor HIGH;
                node A = Terminate, task: a;
                node B = Terminate, deps: [A], executor: HIGH, task: b;
                node PARKED = Pause, deps: [];
                pool W = [Terminate], task: w, policy: P::new(), min: 1, max: 1;
            }
        "#;
        let g = parse_source(src, "t.rs").unwrap().remove(0);
        let out = render(
            &g,
            &Options {
                executors: true,
                ..Default::default()
            },
        );
        let thread = out.find("[\"@THREAD\"]").expect("the @THREAD box: {out}");
        let end = out[thread..].find("\n  end").unwrap() + thread;
        assert!(
            out[thread..end].contains("n_A"),
            "A inherited the default: {out}"
        );
        assert!(!out[thread..end].contains("n_B"), "B keeps HIGH: {out}");
        assert!(
            !out[thread..end].contains("n_PARKED"),
            "a parked node stays with the supervisor: {out}"
        );
        assert!(out.contains("[\"supervisor's executor\"]"), "{out}");
        assert!(out.contains("n_A[\"A<br/>Terminate · task\"]"), "{out}");
        let plain = render(&g, &Options::default());
        for label in [
            "n_A[\"A<br/>Terminate · task · @THREAD\"]",
            "n_B[\"B<br/>Terminate · task · @HIGH\"]",
            "n_PARKED[\"PARKED<br/>Pause · parked\"]",
            "n_W[[\"W<br/>pool ×1 · task · 1..1 · @THREAD\"]]",
        ] {
            assert!(plain.contains(label), "{label}: {plain}");
        }
    }

    #[test]
    fn a_spliced_fragments_nodes_inherit_the_compose_sites_default_executor() {
        let site = r#"
            embassy_supervisor::compose_graph! {
                fragments: [NET_FRAG],
                graph: {
                    default executor THREAD;
                    executor HIGH;
                    node APP = Terminate, deps: [NET ready], task: crate::app::run,
                        reads: [crate::net::STACK];
                    node HB = Pause, deps: [], executor: HIGH, task: crate::hb;
                }
            }
        "#;
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(site, "main.rs").unwrap());
        let (out, warnings) = resolve(decls);
        assert!(warnings.is_empty(), "{warnings:?}");
        let net = out[0]
            .spec
            .items
            .iter()
            .find_map(|i| match i {
                Item::Node(n) if n.ident == "NET" => Some(n),
                _ => None,
            })
            .expect("NET spliced in");
        assert_eq!(
            net.executor.as_ref().map(|e| e.to_string()).as_deref(),
            Some("THREAD")
        );
        assert!(net.executor_defaulted);
        let drawn = render(
            &out[0],
            &Options {
                executors: true,
                ..Default::default()
            },
        );
        let thread = drawn
            .find("[\"@THREAD\"]")
            .expect("the @THREAD box: {drawn}");
        let end = drawn[thread..].find("\n  end").unwrap() + thread;
        assert!(
            drawn[thread..end].contains("n_NET"),
            "NET rides the default: {drawn}"
        );
        assert!(drawn[thread..end].contains("n_APP"), "{drawn}");
        assert!(
            !drawn[thread..end].contains("n_HB"),
            "HB keeps HIGH: {drawn}"
        );
        assert!(
            !drawn.contains("[\"supervisor's executor\"]"),
            "nothing left unrouted: {drawn}"
        );
    }

    #[test]
    fn a_hub_past_max_fanout_collapses_into_one_reader_box() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w, writes: [HUB];
                node A = Terminate, task: a, reads: [HUB];
                node B = Terminate, task: b, reads: [HUB];
                node C = Terminate, task: c, reads: [HUB];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let opts = |n| Options {
            runtime: true,
            max_fanout: n,
            ..Default::default()
        };
        let loose = render(&d[0], &opts(3));
        assert!(
            loose.contains("s_HUB --> n_A") || loose.contains("s_HUB -- "),
            "{loose}"
        );
        assert!(
            !loose.contains("readers:"),
            "at the bound, nothing compacts: {loose}"
        );

        let tight = render(&d[0], &opts(2));
        assert!(
            tight.contains("fan_s_HUB[\"3 readers: A, B, C\"]"),
            "{tight}"
        );
        assert!(tight.contains("s_HUB --> fan_s_HUB"), "{tight}");
        assert!(
            !tight.contains("--> n_A"),
            "the individual reader edges went into the box: {tight}"
        );
        assert!(
            tight.contains("n_W --> s_HUB"),
            "the writer edge never compacts: {tight}"
        );

        let signals = render(
            &d[0],
            &Options {
                signals: true,
                max_fanout: 2,
                ..Default::default()
            },
        );
        assert!(signals.contains("n_W -.-> s_HUB"), "{signals}");
        assert!(signals.contains("s_HUB -.-> fan_s_HUB"), "{signals}");
    }

    #[test]
    fn click_links_send_each_node_to_its_declaration() {
        let out = render(
            &graph(),
            &Options {
                links: Some("editor://{file}:{line}".to_string()),
                ..Default::default()
            },
        );
        let line = out
            .lines()
            .find(|l| l.contains("click n_WATCHDOG"))
            .expect("a click line per node");
        assert!(line.contains("editor://"), "{line}");
        assert!(
            line.contains("editor://t.rs:5\""),
            "the node's own line, not the graph's: {line}"
        );
    }

    #[test]
    fn click_links_in_a_composed_graph_use_the_fragment_source() {
        let http_fragment = r#"embassy_supervisor::supervisor_fragment! {
            name: HTTP_FRAG;
            pool HTTP = [Terminate, OnDemand], deps: [NET ready], task: http,
                policy: Policy::new(), min: 1, max: 2;
        }"#;
        let mut decls = parse_source(FRAGMENT, "net.rs").unwrap();
        decls.extend(parse_source(http_fragment, "http.rs").unwrap());
        decls.extend(parse_source(COMPOSE, "main.rs").unwrap());
        let (resolved, warnings) = resolve(decls);
        assert!(warnings.is_empty(), "{warnings:?}");

        let out = render(
            &resolved[0],
            &Options {
                links: Some("editor://{file}:{line}".to_string()),
                ..Default::default()
            },
        );
        let http = out
            .lines()
            .find(|line| line.contains("click n_HTTP"))
            .expect("a click line for the HTTP pool");
        assert!(http.contains("editor://http.rs:3\""), "{http}");
        assert!(!http.contains("main.rs"), "{http}");
    }

    #[test]
    fn per_node_states_carry_the_concrete_gates() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node NET = Terminate, task: n, slot_timeout: 5000,
                    resources: [HW: Hw], provides: [STACK], writes: [STACK_UP];
                node APP = Pause, deps: [NET ready], task: a, reads: [STACK_UP];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let per_node = states::render(
            &d[0],
            &Options {
                states: true,
                signals: true,
                ..Default::default()
            },
        );
        assert!(per_node.contains("takes HW"), "{per_node}");
        assert!(per_node.contains("within 5000 ms"), "{per_node}");
        assert!(per_node.contains("clears STACK"), "{per_node}");
        assert!(per_node.contains("waits NET ready"), "{per_node}");
        assert!(per_node.contains("· writes STACK_UP"), "{per_node}");
        assert!(per_node.contains("· reads STACK_UP"), "{per_node}");

        let grouped = states::render(
            &d[0],
            &Options {
                states: true,
                ..Default::default()
            },
        );
        assert!(
            !grouped.contains("takes HW"),
            "the default view keeps the shape grouping: {grouped}"
        );
    }

    #[test]
    fn coverage_warns_in_both_directions_but_not_for_dep_surplus() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: tasks::entry, discover;
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let entry = ScannedFn {
            module: vec!["tasks".to_string()],
            rooted: true,
            crate_dir: None,
            name: "entry".to_string(),
        };
        let stray = ScannedFn {
            module: vec!["m".to_string()],
            rooted: true,
            crate_dir: None,
            name: "unused".to_string(),
        };

        let missing = coverage_warnings(&decls, &[], &[], &[]);
        assert!(
            missing
                .iter()
                .any(|w| w.contains("A:") && w.contains("tasks::entry")),
            "{missing:?}"
        );
        assert!(coverage_warnings(&decls, std::slice::from_ref(&entry), &[], &[]).is_empty());
        assert!(
            coverage_warnings(&decls, &[], std::slice::from_ref(&entry), &[]).is_empty(),
            "a dep file satisfies the bound fn too"
        );

        let unbound = coverage_warnings(&decls, &[entry.clone(), stray.clone()], &[], &[]);
        assert!(
            unbound.iter().any(|w| w.contains("m::unused")),
            "{unbound:?}"
        );
        assert!(
            coverage_warnings(&decls, &[entry], &[stray], &[]).is_empty(),
            "a library shipping more fns than one graph uses is not drift"
        );
    }

    #[test]
    fn the_model_dump_holds_the_graph_and_the_scan() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: worker, discover,
                    provides: [#[cfg(feature = "x")] OUT],
                    #[cfg(feature = "x")] disabled;
            }
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode) {
                let v = node.get(&IN);
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let m = model_json(&decls, &discovered);
        let item = &m["graphs"][0]["items"][0];
        assert_eq!(item["name"], "A");
        assert_eq!(item["provides"][0]["name"], "OUT");
        assert_eq!(item["provides"][0]["cfg"][0], "feature=\"x\"");
        assert_eq!(item["disabled"], true);
        assert_eq!(item["disabled_cfg"][0], "feature=\"x\"");
        assert_eq!(item["discover"], true);
        assert_eq!(item["discover_cfg"], serde_json::json!([]));
        assert_eq!(m["discovered"][0]["fn"], "worker");
        assert_eq!(m["discovered"][0]["path"], "IN");
    }

    const FULL_GRAPH: &str = r#"
        embassy_supervisor::supervisor_graph! {
            name: RIG;
            observe writes: it.load(core::sync::atomic::Ordering::Relaxed);
            observe reads: it.get();
            node GNSS = Terminate, task: crate::gnss_task,
                slot_timeout: 5000, ack_timeout: 100,
                beat_timeout: 500, beat_window: 2, ready_on_write,
                exit: Result<(), FixError>, state: u32 = 7, cancel,
                writes: [FIX observed beat],
                resources: [UART: crate::Uart, POWER: divisible],
                dataflow: [#[cfg(feature = "log")] crate::log::emit];
            node PARKED = Terminate, deps: [GNSS ready];
            node RAW = Terminate, spawn: crate::raw_task;
            pool WORKERS = [Terminate, OnDemand], task: crate::worker,
                policy: DeferredShrink = DeferredShrink::new(Duration::from_secs(2)),
                min: 1, max: 2, slot_timeout: 1000, ack_timeout: 50,
                state: zeroed u8, cancel, reads: [FIX];
            #[cfg(feature = "tiers")]
            executor HIGH;
        }
    "#;

    #[test]
    fn the_typed_model_maps_every_node_clause() {
        use model::{ItemModel, TaskKind};
        let decls = parse_source(FULL_GRAPH, "t.rs").unwrap();
        let full = full_model(&decls);
        assert_eq!(full.graphs.len(), 1);
        let g = &full.graphs[0];
        assert_eq!(g.name.as_deref(), Some("RIG"));
        assert_eq!(g.macro_name, "supervisor_graph!");
        let ItemModel::Node(n) = &g.items[0] else {
            panic!("node")
        };
        assert_eq!(n.name, "GNSS");
        let task = n.task.as_ref().unwrap();
        assert_eq!(task.kind, TaskKind::Shell);
        assert_eq!(task.path.replace(' ', ""), "crate::gnss_task");
        let ItemModel::Node(raw) = &g.items[2] else {
            panic!("node")
        };
        assert_eq!(raw.task.as_ref().unwrap().kind, TaskKind::Spawn);
        assert_eq!(n.slot_timeout_ms.as_ref().unwrap().as_u64(), Some(5000));
        assert_eq!(n.ack_timeout_ms.as_ref().unwrap().as_u64(), Some(100));
        assert_eq!(n.beat_timeout_ms.as_ref().unwrap().as_u64(), Some(500));
        assert_eq!(n.beat_window.as_ref().unwrap().as_u64(), Some(2));
        assert!(n.ready_on_write.is_some());
        assert_eq!(
            n.exit.as_ref().unwrap().text.replace(' ', ""),
            "Result<(),FixError>"
        );
        let st = n.state.as_ref().unwrap();
        assert_eq!(st.ty.text, "u32");
        assert_eq!(st.init, "7");
        assert!(n.cancel);
        let ItemModel::Node(parked) = &g.items[1] else {
            panic!("node")
        };
        assert!(parked.task.is_none());
        assert!(!parked.cancel);
        assert_eq!(
            g.observe_writes.as_ref().unwrap().text.replace(' ', ""),
            "it.load(core::sync::atomic::Ordering::Relaxed)"
        );
        assert_eq!(
            g.observe_reads.as_ref().unwrap().text.replace(' ', ""),
            "it.get()"
        );
        assert_eq!(
            n.resources[0].ty.as_ref().unwrap().text.replace(' ', ""),
            "crate::Uart"
        );
        assert!(n.resources[1].divisible && n.resources[1].ty.is_none());
        assert_eq!(n.dataflow[0].path, "crate::log::emit");
        assert_eq!(n.dataflow[0].cfg, ["feature=\"log\""]);
        let ItemModel::Executor(x) = &g.items[4] else {
            panic!("executor")
        };
        assert_eq!(x.cfg, ["feature=\"tiers\""]);
    }

    #[test]
    fn the_typed_model_maps_every_pool_clause() {
        use model::{ItemModel, TaskKind};
        let decls = parse_source(FULL_GRAPH, "t.rs").unwrap();
        let full = full_model(&decls);
        let ItemModel::Pool(p) = &full.graphs[0].items[3] else {
            panic!("pool")
        };
        assert_eq!(p.name, "WORKERS");
        assert_eq!(p.task.kind, TaskKind::Shell);
        assert_eq!(p.task.path.replace(' ', ""), "crate::worker");
        assert_eq!(p.policy_ty.as_ref().unwrap().text, "DeferredShrink");
        assert!(
            p.policy
                .text
                .replace(' ', "")
                .starts_with("DeferredShrink::new")
        );
        assert_eq!(p.min.text, "1");
        assert_eq!(p.max.text, "2");
        assert_eq!(p.slot_timeout_ms.as_ref().unwrap().as_u64(), Some(1000));
        assert_eq!(p.ack_timeout_ms.as_ref().unwrap().as_u64(), Some(50));
        let st = p.state.as_ref().unwrap();
        assert_eq!(st.ty.text, "u8");
        assert_eq!(st.init, "zeroed");
        assert!(p.cancel);
    }

    #[test]
    fn the_json_dump_projects_the_typed_model() {
        use model::ItemModel;
        let decls = parse_source(FULL_GRAPH, "t.rs").unwrap();
        let full = full_model(&decls);
        let m = model_json(&decls, &[]);
        let ItemModel::Node(n) = &full.graphs[0].items[0] else {
            panic!("node")
        };
        let j = &m["graphs"][0]["items"][0];
        assert_eq!(
            j["slot_timeout_ms"].as_u64(),
            n.slot_timeout_ms.as_ref().unwrap().as_u64()
        );
        assert_eq!(
            j["ack_timeout_ms"].as_u64(),
            n.ack_timeout_ms.as_ref().unwrap().as_u64()
        );
        assert_eq!(
            j["beat_timeout_ms"].as_u64(),
            n.beat_timeout_ms.as_ref().unwrap().as_u64()
        );
        assert_eq!(
            j["beat_window"].as_u64(),
            n.beat_window.as_ref().unwrap().as_u64()
        );
        assert_eq!(j["ready_on_write"], n.ready_on_write.is_some());
        assert_eq!(j["task"]["kind"], "task");
        assert_eq!(m["graphs"][0]["items"][2]["task"]["kind"], "spawn");
        assert_eq!(j["task"]["path"], n.task.as_ref().unwrap().path);
        assert_eq!(j["exit"], n.exit.as_ref().unwrap().text);
        assert_eq!(j["state"]["ty"], n.state.as_ref().unwrap().ty.text);
        assert_eq!(j["cancel"], n.cancel);
        assert_eq!(m["graphs"][0]["items"][1]["parked"], true);
        assert_eq!(m["graphs"][0]["items"][1]["task"], serde_json::Value::Null);
        assert_eq!(
            j["resources"][0]["ty"],
            n.resources[0].ty.as_ref().unwrap().text
        );
        assert_eq!(j["resources"][1]["ty"], serde_json::Value::Null);
        assert_eq!(j["dataflow"][0], "crate::log::emit");
        assert_eq!(j["dataflow_cfg"][0][0], "feature=\"log\"");
        assert_eq!(m["graphs"][0]["items"][4]["cfg"][0], "feature=\"tiers\"");
        assert_eq!(
            m["graphs"][0]["observe_reads"]
                .as_str()
                .unwrap()
                .replace(' ', ""),
            "it.get()"
        );
        let ItemModel::Pool(p) = &full.graphs[0].items[3] else {
            panic!("pool")
        };
        let jp = &m["graphs"][0]["items"][3];
        assert_eq!(jp["min"], p.min.text);
        assert_eq!(jp["max"], p.max.text);
        assert_eq!(jp["policy"], p.policy.text);
        assert_eq!(jp["policy_ty"], p.policy_ty.as_ref().unwrap().text);
        assert_eq!(jp["task"]["kind"], "task");
        assert_eq!(
            jp["slot_timeout_ms"].as_u64(),
            p.slot_timeout_ms.as_ref().unwrap().as_u64()
        );
        assert_eq!(
            jp["ack_timeout_ms"].as_u64(),
            p.ack_timeout_ms.as_ref().unwrap().as_u64()
        );
        assert_eq!(jp["state"]["init"], "zeroed");
        assert_eq!(jp["cancel"], p.cancel);
    }

    #[test]
    fn an_indexed_element_links_to_its_whole_array_box() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w, writes: [crate::sig::ARR[0]];
                node R = Terminate, task: r, reads: [crate::sig::ARR];
                node X = Terminate, task: x, reads: [crate::sig::OTHER];
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let out = render(
            &d[0],
            &Options {
                runtime: true,
                ..Default::default()
            },
        );
        assert!(
            out.contains("-. \"element of\" .-"),
            "the containment draws: {out}"
        );
        assert!(
            out.matches("element of").count() == 1,
            "only the indexed pair links: {out}"
        );
    }

    #[test]
    fn a_cfg_attr_wrapped_dataflow_fn_is_scanned_and_drawn_with_its_predicate() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node U = Terminate, task: crate::worker, discover;
            }
            #[cfg_attr(feature = "grown", embassy_supervisor::dataflow)]
            async fn worker(node: &'static TaskNode) {
                let mut rx = node.reader(&crate::LATEST).receiver().unwrap();
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut scanned = Vec::new();
        scan_fns(&parsed(src), "t.rs", &mut scanned);
        assert_eq!(
            scanned,
            [ScannedFn {
                module: vec!["t".to_string()],
                rooted: false,
                crate_dir: None,
                name: "worker".to_string()
            }]
        );
        assert!(coverage_warnings(&d, &scanned, &[], &[]).is_empty());

        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered,
                ..Default::default()
            },
        );
        assert!(text.contains("s_crate__LATEST"), "{text}");
        assert!(
            text.contains(
                "s_crate__LATEST -- \"discovered · <small>cfg(feature=#quot;grown#quot;)</small>\" --> n_U"
            ),
            "the read edge carries the cfg_attr predicate, once: {text}"
        );
    }

    #[test]
    fn a_bundle_adoption_resolves_to_its_members() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node USER = Terminate, task: worker, dataflow: [crate::api::BUNDLE];
            }
            #[embassy_supervisor::dataflow_bundle]
            pub mod api {
                #[embassy_supervisor::dataflow]
                pub fn set_a(node: &'static TaskNode, v: u32) {
                    node.put(&crate::A, v);
                }
                #[embassy_supervisor::dataflow]
                pub fn read_b(node: &'static TaskNode) -> u32 {
                    node.get(&crate::B)
                }
                pub fn plain() {}
            }
        "#;
        let d = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(&parsed(src), "t.rs", &mut discovered);
        let mut bundles = Vec::new();
        scan_bundles(&parsed(src), "t.rs", &mut bundles);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].name, "BUNDLE");
        assert_eq!(bundles[0].fns, ["set_a", "read_b"]);
        assert_eq!(bundles[0].path, ["t", "api"]);

        let text = render(
            &d[0],
            &Options {
                runtime: true,
                discovered,
                bundles: bundles.clone(),
                ..Default::default()
            },
        );
        assert!(
            text.contains("n_USER -- \"discovered\" --> s_crate__A"),
            "{text}"
        );
        assert!(
            text.contains("s_crate__B -- \"discovered\" --> n_USER"),
            "{text}"
        );

        let mut scanned = Vec::new();
        scan_fns(&parsed(src), "t.rs", &mut scanned);
        assert!(coverage_warnings(&d, &scanned, &[], &bundles).is_empty());
        assert!(
            !coverage_warnings(&d, &scanned, &[], &[])
                .iter()
                .filter(|w| w.contains("BUNDLE"))
                .collect::<Vec<_>>()
                .is_empty(),
            "without the bundle scan the adoption is an unresolved fn"
        );
    }
}
