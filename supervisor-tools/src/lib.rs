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
#[cfg(feature = "cli")]
pub mod inputs;
/// Linting: orphan reads, dead writes and public gates.
pub mod lint;
pub mod model;
/// Output helpers: mermaid.live links, HTML pages, and markdown updates.
#[cfg(feature = "cli")]
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
/// stem, then the inline `mod`s around the fn. It resolves `use` aliases
/// (from the access's own module outward) and marks relative paths;
/// qualification and binding happen in `render::discovered_key` and
/// `binding_for`.
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
    let aliases = use_aliases(file, &path);
    for d in &mut out[start..] {
        let scopes: Vec<&Aliases> = (0..=d.module.len())
            .rev()
            .filter_map(|n| aliases.get(&d.module[..n]))
            .collect();
        (d.access.path, d.relative) = resolve_path(&d.access.path, &scopes);
    }
}

/// Every `#[dataflow]` fn's accesses, inline modules included, in the order
/// `embassy_supervisor_syntax::scan_dataflow` visits them. Paths come back as
/// written: [`scan_source`] resolves them and sets `relative`.
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

/// A module's `use` imports: the name in scope, then the path it names.
type Aliases = std::collections::BTreeMap<String, String>;

/// Every module's `use` imports, keyed by module path: the file's own
/// (`root`), then one key per inline `mod` below it.
fn use_aliases(
    file: &syn::File,
    root: &[String],
) -> std::collections::BTreeMap<Vec<String>, Aliases> {
    fn collect(tree: &syn::UseTree, prefix: &mut Vec<String>, map: &mut Aliases) {
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
    fn walk(
        items: &[syn::Item],
        module: &mut Vec<String>,
        out: &mut std::collections::BTreeMap<Vec<String>, Aliases>,
    ) {
        for item in items {
            match item {
                syn::Item::Use(u) => collect(
                    &u.tree,
                    &mut Vec::new(),
                    out.entry(module.clone()).or_default(),
                ),
                syn::Item::Mod(m) => {
                    if let Some((_, items)) = &m.content {
                        module.push(m.ident.to_string());
                        walk(items, module, out);
                        module.pop();
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(&file.items, &mut root.to_vec(), &mut out);
    out
}

/// `path` with a leading `use` alias expanded (the first of `scopes`, innermost
/// module first, that imports it), and whether the result is relative.
fn resolve_path(path: &str, scopes: &[&Aliases]) -> (String, bool) {
    let (head, index) = match path.find('[') {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    let mut segs: Vec<&str> = head.split("::").collect();
    let explicit_self = segs.first() == Some(&"self");
    if explicit_self {
        segs.remove(0);
    }
    if let Some(full) = segs
        .first()
        .and_then(|s| scopes.iter().find_map(|a| a.get(*s)))
    {
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
/// Fragment declarations of one name under different `#[cfg]` sets are build
/// alternatives: every one is spliced, each item gated by its fragment's
/// predicates. Only declarations with identical `#[cfg]` sets are ambiguous.
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
        let mut fragment_crate_dirs = std::collections::BTreeMap::new();
        for want in &site.fragments {
            let tail = want.rsplit("::").next().unwrap_or(want);
            let mut groups: Vec<Vec<&Decl>> = Vec::new();
            for d in decls
                .iter()
                .filter(|d| d.kind == DeclKind::Fragment && d.name().as_deref() == Some(tail))
            {
                match groups.iter_mut().find(|g| same_cfgs(&g[0].cfg, &d.cfg)) {
                    Some(group) => group.push(d),
                    None => groups.push(vec![d]),
                }
            }
            if groups.is_empty() {
                warnings.push(format!(
                    "{}:{}: fragment `{want}` is not among the files given, so its \
                     nodes are drawn only where something depends on them; pass the \
                     file declaring it for the whole graph",
                    site.origin, site.line
                ));
                continue;
            }
            for group in groups {
                let frag = match group.as_slice() {
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
                        fragment_origins
                            .entry(tail.to_string())
                            .or_insert_with(|| frag.origin.clone());
                        if let Some(dir) = &frag.crate_dir {
                            fragment_crate_dirs
                                .entry(tail.to_string())
                                .or_insert_with(|| dir.clone());
                        }
                        items.append(&mut resolved);
                    }
                    Err(e) => warnings.push(format!("{}:{}: {e}", frag.origin, frag.line)),
                }
            }
        }
        items.extend(site.spec.items.iter().cloned());
        if let Some(ex) = &site.spec.default_executor {
            apply_default_executor(&mut items, ex);
        }
        let mut merged = site.clone();
        merged.spec.items = items;
        merged.fragment_origins = fragment_origins;
        merged.fragment_crate_dirs = fragment_crate_dirs;
        out.push(merged);
    }

    for d in &decls {
        let drawn_inside = d.kind == DeclKind::Compose
            || (d.kind == DeclKind::Fragment
                && consumed.iter().any(|(o, l)| *o == d.origin && *l == d.line));
        if !drawn_inside {
            out.push(d.clone());
        }
    }
    out.sort_by_key(|d| (d.origin.clone(), d.line));
    (out, warnings)
}

/// Do two declarations carry the same `#[cfg]` predicates, in any order?
fn same_cfgs(a: &[String], b: &[String]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    a == b
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
    // A gated fragment declaration contributes its items only where its
    // predicates hold: they carry them in front of their own.
    let gates: Vec<syn::Attribute> = frag
        .cfg
        .iter()
        .filter_map(|p| {
            let pred = TokenStream::from_str(p).ok()?;
            Some(syn::parse_quote!(#[cfg(#pred)]))
        })
        .collect();
    Ok(spec
        .items
        .into_iter()
        .map(|mut item| {
            let cfg = match &mut item {
                Item::Node(n) => {
                    n.fragment = Some(tail.to_string());
                    &mut n.cfg
                }
                Item::Pool(p) => {
                    p.fragment = Some(tail.to_string());
                    &mut p.cfg
                }
                Item::Executor(x) => &mut x.cfg,
            };
            cfg.splice(0..0, gates.iter().cloned());
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
                        crate_dir: bundle.crate_dir.clone().or_else(|| b.crate_dir.clone()),
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
    /// The crate directory of the declaring file, when known: the crate an
    /// `absolute` path means.
    pub(crate) crate_dir: Option<String>,
    /// The `#[cfg]` texts on the `discover` clause or the adoption.
    pub(crate) cfg: Vec<String>,
}

impl FnBinding {
    fn from_path(
        path: &syn::Path,
        cfg: &[syn::Attribute],
        crate_dir: Option<&str>,
    ) -> Option<Self> {
        let mut module: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let name = module.pop()?;
        let absolute = module.first().is_some_and(|s| s == "crate");
        module.retain(|s| !matches!(s.as_str(), "crate" | "self" | "super"));
        Some(Self {
            name,
            module,
            absolute,
            crate_dir: crate_dir.map(str::to_string),
            cfg: model::cfg_texts(cfg),
        })
    }

    /// Can a scanned fn at `module` be the one this binding names?
    /// * written `crate::`, the paths are in different crates when both crate
    ///   directories are known: never;
    /// * written `crate::` and the file is under `src/`: the paths must be equal;
    /// * otherwise the binding's segments must end the module path;
    /// * or, for a path not written `crate::`, one leading segment more names the crate
    ///   (`common::tasks::rate::entry` for `common/src/tasks/rate.rs`). That form reaches a
    ///   crate-root fn only when the file's crate directory is known and equals that segment,
    ///   so `imu::run` never binds the root `fn run` beside it.
    fn module_matches(&self, module: &[String], rooted: bool, crate_dir: Option<&str>) -> bool {
        if self.absolute
            && let (Some(ours), Some(theirs)) = (self.crate_dir.as_deref(), crate_dir)
            && ours != theirs
        {
            return false;
        }
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

/// The fns `item` of `decl` binds, bundles expanded. A `crate::` path means
/// the crate of the file that declared the item: its fragment's, for an item
/// spliced from one.
pub(crate) fn decl_bindings(decl: &Decl, item: &Item, bundles: &[Bundle]) -> Vec<FnBinding> {
    let crate_dir = render::fragment_of(item)
        .and_then(|f| decl.fragment_crate_dirs.get(f))
        .or(decl.crate_dir.as_ref());
    expand_bundles(fn_bindings(item, crate_dir.map(String::as_str)), bundles)
}

/// The fns `item` binds. Its task fn counts only under `discover`: that is
/// the only case the macro takes the fn's tables.
fn fn_bindings(item: &Item, crate_dir: Option<&str>) -> Vec<FnBinding> {
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
        .filter_map(|(path, cfg)| FnBinding::from_path(path, cfg, crate_dir))
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
    for decl in decls {
        for item in &decl.spec.items {
            let Some(node) = item_name(item) else {
                continue;
            };
            for b in decl_bindings(decl, item, bundles) {
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
/// tooling that is not a diagram. It projects [`full_model`] field for field,
/// under a top-level `"schema"` version that changes when a field is removed
/// or changes meaning.
#[cfg(feature = "cli")]
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
                        "fragment": n.fragment,
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
                        "fragment": p.fragment,
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
                "cfg": g.cfg,
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
    serde_json::json!({ "schema": 1, "graphs": graphs, "discovered": accesses })
}

#[cfg(test)]
mod tests;
