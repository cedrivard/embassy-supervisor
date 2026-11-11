//! Collecting Rust source files to scan.

use crate::{Bundle, Decl, Discovered};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Collect `.rs` files from command-line arguments.
///
/// Directories are walked recursively, skipping hidden directories, symlinks,
/// and a `target` directory that carries Cargo's `CACHEDIR.TAG`. `-` is
/// returned as-is to mean stdin. A directory that cannot be read is reported
/// in `warnings`.
pub fn collect_inputs(args: &[String], warnings: &mut Vec<String>) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for a in args {
        if a == "-" {
            out.push(PathBuf::from("-"));
            continue;
        }
        let p = PathBuf::from(a);
        if p.is_dir() {
            walk_dir(&p, &mut out, warnings);
        } else if p.is_file() {
            out.push(p);
        } else {
            return Err(format!("{a}: no such file or directory"));
        }
    }
    Ok(dedup(out))
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>, warnings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!(
                "{}: not read ({e}); the files under it are not scanned",
                dir.display()
            ));
            return;
        }
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let is_link = path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if path.is_dir() {
            // Cargo tags its build directories; a source directory that
            // happens to be named `target` is walked like any other.
            let build_dir = name == "target" && path.join("CACHEDIR.TAG").is_file();
            if !build_dir && !name.starts_with('.') && !is_link {
                walk_dir(&path, out, warnings);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Find the default input files for the crate in the current working directory.
///
/// Walks `src/main.rs`, `src/lib.rs`, `src/bin/*.rs`, and their declared file
/// modules. A file whose `mod` declarations cannot be followed is reported in
/// `warnings`.
pub fn crate_default_inputs(warnings: &mut Vec<String>) -> Result<Vec<PathBuf>, String> {
    let dir = manifest_dir()?;
    let src = dir.join("src");
    let mut roots = Vec::new();
    for cand in ["main.rs", "lib.rs"] {
        let p = src.join(cand);
        if p.is_file() {
            roots.push(p);
        }
    }
    if let Ok(bins) = std::fs::read_dir(src.join("bin")) {
        for e in bins.flatten() {
            let p = e.path();
            if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                roots.push(p);
            } else if p.is_dir() {
                let main = p.join("main.rs");
                if main.is_file() {
                    roots.push(main);
                }
            }
        }
    }
    if roots.is_empty() {
        return Err(format!(
            "no input files, and {} has no src/main.rs or src/lib.rs — pass \
             files or directories explicitly (see --help)",
            dir.display()
        ));
    }
    let mut out = Vec::new();
    for r in &roots {
        out.push(r.clone());
        expand_modules(r, true, &mut out, warnings);
    }
    Ok(dedup(out))
}

fn manifest_dir() -> Result<PathBuf, String> {
    let mut dir = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        if dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(
                "no input files and no Cargo.toml above the working directory; \
                 pass files or directories explicitly (see --help)"
                    .to_string(),
            );
        }
    }
}

/// Recursively expand `mod foo;` declarations in `file` into concrete paths,
/// inline `mod a { mod b; }` blocks included.
///
/// `is_root` marks a crate root (`lib.rs`, `main.rs`, a `src/bin/` file),
/// whose modules sit beside it whatever its name. Discovered files are
/// appended to `out`, then expanded in turn. A file `syn` cannot parse is
/// reported in `warnings`, since its `mod` declarations are not followed.
pub fn expand_modules(
    file: &Path,
    is_root: bool,
    out: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) {
    let Ok(src) = std::fs::read_to_string(file) else {
        return;
    };
    let parsed = match syn::parse_file(&src) {
        Ok(parsed) => parsed,
        Err(e) => {
            warnings.push(format!(
                "{}: not parsed ({e}); its `mod` declarations are not followed",
                file.display()
            ));
            return;
        }
    };
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let parent = file.parent().unwrap_or(Path::new("."));
    let base = if is_root || stem == "mod" {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    follow_mods(&parsed.items, parent, &base, out, warnings);
}

/// Follow the `mod` declarations in `items`. `base` is the directory a plain
/// `mod name;` resolves in; `path_dir` is the one a `#[path]` resolves in
/// (the file's own directory at its top level, `base` inside inline modules).
fn follow_mods(
    items: &[syn::Item],
    path_dir: &Path,
    base: &Path,
    out: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        let name = m.ident.to_string();
        if let Some((_, inner)) = &m.content {
            let dir = base.join(&name);
            follow_mods(inner, &dir, &dir, out, warnings);
            continue;
        }
        let explicit = m.attrs.iter().find_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(path_dir.join(s.value())),
                _ => None,
            },
            _ => None,
        });
        let cands = match explicit {
            Some(p) => vec![p],
            None => vec![
                base.join(format!("{name}.rs")),
                base.join(&name).join("mod.rs"),
            ],
        };
        for cand in cands {
            if cand.is_file() && !out.contains(&cand) {
                out.push(cand.clone());
                expand_modules(&cand, false, out, warnings);
                break;
            }
        }
    }
}

/// Find `.rs` files in the current package's workspace path dependencies.
///
/// Runs `cargo metadata --no-deps` (no resolution, no network) and walks the
/// `src/` directory of every workspace package the current package reaches
/// through `path` dependencies, transitively. In a virtual manifest's
/// directory, where there is no current package, every member is walked.
pub fn dependency_sources(warnings: &mut Vec<String>) -> Result<Vec<PathBuf>, String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("running `cargo metadata`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let shown = stderr
            .lines()
            .find(|l| l.starts_with("error"))
            .map_or_else(|| stderr.trim().to_string(), str::to_string);
        return Err(format!("`cargo metadata` failed: {shown}"));
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata json: {e}"))?;
    let own = manifest_dir().ok().map(|d| d.join("Cargo.toml"));
    let mut files = Vec::new();
    for dir in path_dependency_dirs(&meta, own.as_deref()) {
        let src = dir.join("src");
        if src.is_dir() {
            walk_dir(&src, &mut files, warnings);
        }
    }
    Ok(dedup(files))
}

/// The package directories `cargo metadata --no-deps` output reaches from the
/// package whose manifest is `own`, through `path` dependencies, excluding
/// `own` itself; every package when `own` is none of them.
fn path_dependency_dirs(meta: &serde_json::Value, own: Option<&Path>) -> Vec<PathBuf> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let packages: Vec<&serde_json::Value> =
        meta["packages"].as_array().into_iter().flatten().collect();
    let dirs: Vec<Option<PathBuf>> = packages
        .iter()
        .map(|p| {
            let manifest = Path::new(p["manifest_path"].as_str()?);
            Some(canon(manifest.parent()?))
        })
        .collect();
    let own_dir = own.and_then(Path::parent).map(canon);
    let start = own_dir.and_then(|d| dirs.iter().position(|x| x.as_ref() == Some(&d)));
    let reached: Vec<usize> = match start {
        None => (0..packages.len()).collect(),
        Some(start) => {
            let mut seen = vec![false; packages.len()];
            seen[start] = true;
            let mut stack = vec![start];
            let mut reached = Vec::new();
            while let Some(i) = stack.pop() {
                for dep in packages[i]["dependencies"].as_array().into_iter().flatten() {
                    let Some(path) = dep["path"].as_str() else {
                        continue;
                    };
                    let path = canon(Path::new(path));
                    if let Some(j) = dirs.iter().position(|d| d.as_ref() == Some(&path))
                        && !seen[j]
                    {
                        seen[j] = true;
                        reached.push(j);
                        stack.push(j);
                    }
                }
            }
            reached.sort_unstable();
            reached
        }
    };
    reached
        .into_iter()
        .filter_map(|i| dirs[i].clone())
        .collect()
}

/// Input file lists for a tool run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    /// Files that may contain graph declarations, as absolute paths (`-` is stdin).
    pub graph: Vec<String>,
    /// Files scanned only for `#[dataflow]` accesses, not declarations, as
    /// absolute paths; none of them is also in `graph`.
    pub scan_only: Vec<String>,
    /// What gathering could not cover: an unreadable directory, a file whose
    /// `mod` declarations could not be followed.
    pub warnings: Vec<String>,
}

/// Resolve command-line file arguments into [`Sources`].
///
/// If `files` is empty, the current crate's sources are used. If `deps` is
/// true, workspace path dependencies are added as scan-only sources. Prints
/// nothing: a caller that re-gathers (`--watch`) decides what to say.
pub fn gather(files: &[String], deps: bool) -> Result<Sources, String> {
    let mut warnings = Vec::new();
    let graph = if files.is_empty() {
        crate_default_inputs(&mut warnings)?
    } else {
        collect_inputs(files, &mut warnings)?
    };
    let graph: Vec<PathBuf> = graph.into_iter().map(absolute_input).collect();
    let scan_only = if deps {
        let in_graph: BTreeSet<PathBuf> = graph.iter().map(|p| canonical(p)).collect();
        dependency_sources(&mut warnings)?
            .into_iter()
            .map(absolute_input)
            .filter(|p| !in_graph.contains(&canonical(p)))
            .collect()
    } else {
        Vec::new()
    };
    let strings = |v: Vec<PathBuf>| v.iter().map(|p| p.display().to_string()).collect();
    Ok(Sources {
        graph: strings(graph),
        scan_only: strings(scan_only),
        warnings,
    })
}

fn absolute_input(p: PathBuf) -> PathBuf {
    if p == Path::new("-") {
        return p;
    }
    std::path::absolute(&p).unwrap_or(p)
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// A file path as diagnostics and diagrams show it: relative to the working
/// directory when the file is under it, as given otherwise.
fn shown(file: &str) -> String {
    let path = Path::new(file);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| file.to_string())
}

/// The result of scanning a set of input sources.
#[derive(Default)]
pub struct Scan {
    /// Parsed graph declarations.
    pub decls: Vec<Decl>,
    /// Discovered `#[dataflow]` accesses in graph sources and dependencies.
    pub scanned_accesses: Vec<Discovered>,
    /// `#[dataflow]` fns in graph sources, with their module paths.
    pub fns: Vec<crate::ScannedFn>,
    /// `#[dataflow]` fns in dependency sources, with their module paths.
    pub dep_fns: Vec<crate::ScannedFn>,
    /// Discovered `#[dataflow_bundle]` modules.
    pub bundles: Vec<Bundle>,
    /// Gate-wrapped statics (`Backed`, `Leased`, `VetoGate`) in every scanned
    /// source, `--deps` sources included.
    pub gate_statics: Vec<crate::GateStatic>,
    /// What the scan could not cover: the [`Sources`] warnings, then every
    /// file `syn` could not parse, whose `#[dataflow]` fns and gates are
    /// missing from the model. Both binaries print these and fail on them.
    pub warnings: Vec<String>,
}

/// Scan all sources and return the collected declarations, accesses, and bundles.
///
/// Each file is lexed once; the declaration finder and the item parser share
/// the token stream. A file that does not lex or parse as Rust is reported in
/// [`Scan::warnings`], except that a graph source that does not lex is an
/// error, since its declarations cannot be found.
pub fn scan(sources: &Sources) -> Result<Scan, String> {
    let mut out = Scan {
        warnings: sources.warnings.clone(),
        ..Scan::default()
    };
    for (file, graph_source) in sources
        .graph
        .iter()
        .map(|f| (f, true))
        .chain(sources.scan_only.iter().map(|f| (f, false)))
    {
        let src = if file == "-" {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| format!("reading stdin: {e}"))?;
            buf
        } else {
            std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?
        };
        let shown = shown(file);
        let unparsed = |e: &dyn std::fmt::Display| {
            format!("{shown}: not parsed ({e}); its #[dataflow] fns and gates are not scanned")
        };
        let tokens = match crate::find::lex(&src, &shown) {
            Ok(tokens) => tokens,
            Err(e) if graph_source => return Err(e.to_string()),
            Err(e) => {
                out.warnings.push(unparsed(&e.message));
                continue;
            }
        };
        if graph_source {
            let mut decls =
                crate::find::parse_tokens(tokens.clone(), &shown).map_err(|e| e.to_string())?;
            // `shown` may have lost the directory above `src/`; the stored
            // path has it.
            let crate_dir = crate::file_module(file).crate_dir;
            for d in &mut decls {
                d.crate_dir.clone_from(&crate_dir);
            }
            out.decls.extend(decls);
        }
        let parsed = match syn::parse2::<syn::File>(tokens) {
            Ok(parsed) => parsed,
            Err(e) => {
                out.warnings.push(unparsed(&e));
                continue;
            }
        };
        crate::scan_source(&parsed, file, &mut out.scanned_accesses);
        crate::scan_gate_statics(&parsed, &shown, &mut out.gate_statics);
        crate::scan_fns(
            &parsed,
            file,
            if graph_source {
                &mut out.fns
            } else {
                &mut out.dep_fns
            },
        );
        crate::scan_bundles(&parsed, file, &mut out.bundles);
    }
    Ok(out)
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(canonical(p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_walk_follows_file_modules() {
        let dir = std::env::temp_dir().join(format!("svm_mods_{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("deep")).unwrap();
        std::fs::create_dir_all(src.join("inline")).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "mod a;\nmod deep;\nmod inline { mod c; }\n",
        )
        .unwrap();
        std::fs::write(src.join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(src.join("deep").join("mod.rs"), "mod b;\n").unwrap();
        std::fs::write(src.join("deep").join("b.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(src.join("inline").join("c.rs"), "pub fn c() {}\n").unwrap();

        let root = src.join("main.rs");
        let mut out = vec![root.clone()];
        let mut warnings = Vec::new();
        expand_modules(&root, true, &mut out, &mut warnings);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(&src).unwrap().display().to_string())
            .collect();
        assert_eq!(
            names,
            ["main.rs", "a.rs", "deep/mod.rs", "deep/b.rs", "inline/c.rs"]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bin_root_finds_its_modules_beside_it() {
        let dir = std::env::temp_dir().join(format!("svm_bin_{}", std::process::id()));
        let bin = dir.join("src").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tool.rs"), "mod helper;\n").unwrap();
        std::fs::write(bin.join("helper.rs"), "mod nested;\n").unwrap();
        std::fs::create_dir_all(bin.join("helper")).unwrap();
        std::fs::write(bin.join("helper").join("nested.rs"), "").unwrap();

        let root = bin.join("tool.rs");
        let mut out = vec![root.clone()];
        let mut warnings = Vec::new();
        expand_modules(&root, true, &mut out, &mut warnings);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(&bin).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["tool.rs", "helper.rs", "helper/nested.rs"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unparsable_file_is_reported_not_skipped() {
        let dir = std::env::temp_dir().join(format!("svm_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.rs");
        std::fs::write(&bad, "mod a;\nfn broken( {}\n").unwrap();
        let mut warnings = Vec::new();
        expand_modules(&bad, true, &mut Vec::new(), &mut warnings);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not followed"), "{warnings:?}");

        let unparsed = dir.join("unparsed.rs");
        std::fs::write(&unparsed, "fn broken(x: ) {}\n").unwrap();
        let sources = Sources {
            scan_only: vec![unparsed.display().to_string()],
            ..Sources::default()
        };
        let scan = scan(&sources).unwrap();
        assert_eq!(scan.warnings.len(), 1, "{:?}", scan.warnings);
        assert!(
            scan.warnings[0].contains("unparsed.rs: not parsed (")
                && scan.warnings[0].contains("are not scanned"),
            "{:?}",
            scan.warnings
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_inputs_walk_and_skip() {
        let dir = std::env::temp_dir().join(format!("svm_walk_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub").join("target")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("x.rs"), "").unwrap();
        std::fs::write(dir.join("sub").join("y.rs"), "").unwrap();
        std::fs::write(dir.join("sub").join("target").join("t.rs"), "").unwrap();
        std::fs::write(dir.join("target").join("CACHEDIR.TAG"), "").unwrap();
        std::fs::write(dir.join("target").join("z.rs"), "").unwrap();
        std::fs::write(dir.join(".git").join("g.rs"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();

        let mut warnings = Vec::new();
        let got = collect_inputs(&[dir.display().to_string()], &mut warnings).unwrap();
        let mut names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["t.rs", "x.rs", "y.rs"],
            "an untagged `target` is source"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(collect_inputs(&["nope-does-not-exist".into()], &mut warnings).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deps_follow_path_dependencies_of_the_current_package_only() {
        let meta = serde_json::json!({
            "packages": [
                { "manifest_path": "/ws/app/Cargo.toml",
                  "dependencies": [
                      { "name": "common", "path": "/ws/common" },
                      { "name": "serde" } ] },
                { "manifest_path": "/ws/common/Cargo.toml",
                  "dependencies": [ { "name": "leaf", "path": "/ws/leaf" } ] },
                { "manifest_path": "/ws/leaf/Cargo.toml", "dependencies": [] },
                { "manifest_path": "/ws/other/Cargo.toml", "dependencies": [] },
            ]
        });
        let got = path_dependency_dirs(&meta, Some(Path::new("/ws/app/Cargo.toml")));
        assert_eq!(
            got,
            [PathBuf::from("/ws/common"), PathBuf::from("/ws/leaf")]
        );
        let virtual_root = path_dependency_dirs(&meta, Some(Path::new("/ws/Cargo.toml")));
        assert_eq!(virtual_root.len(), 4, "{virtual_root:?}");
    }
}
