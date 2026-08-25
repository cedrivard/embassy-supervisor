//! Collecting Rust source files to scan.

use crate::{Bundle, Decl, Discovered};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Collect `.rs` files from command-line arguments.
///
/// Directories are walked recursively (skipping `target`, hidden directories,
/// and symlinks). `-` is returned as-is to mean stdin.
pub fn collect_inputs(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for a in args {
        if a == "-" {
            out.push(PathBuf::from("-"));
            continue;
        }
        let p = PathBuf::from(a);
        if p.is_dir() {
            walk_dir(&p, &mut out);
        } else if p.is_file() {
            out.push(p);
        } else {
            return Err(format!("{a}: no such file or directory"));
        }
    }
    Ok(dedup(out))
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
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
            if name != "target" && !name.starts_with('.') && !is_link {
                walk_dir(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Find the default input files for the crate in the current working directory.
///
/// Walks `src/main.rs`, `src/lib.rs`, `src/bin/*.rs`, and their declared file
/// modules.
pub fn crate_default_inputs() -> Result<Vec<PathBuf>, String> {
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
        expand_modules(r, &mut out);
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

/// Recursively expand `mod foo;` declarations in `file` into concrete paths.
///
/// Discovered files are appended to `out`, then expanded in turn.
pub fn expand_modules(file: &Path, out: &mut Vec<PathBuf>) {
    let Ok(src) = std::fs::read_to_string(file) else {
        return;
    };
    let Ok(parsed) = syn::parse_file(&src) else {
        return;
    };
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let parent = file.parent().unwrap_or(Path::new("."));
    let base = if matches!(stem, "mod" | "lib" | "main") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    for item in &parsed.items {
        let syn::Item::Mod(m) = item else { continue };
        if m.content.is_some() {
            continue;
        }
        let explicit = m.attrs.iter().find_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(parent.join(s.value())),
                _ => None,
            },
            _ => None,
        });
        let name = m.ident.to_string();
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
                expand_modules(&cand, out);
                break;
            }
        }
    }
}

/// Find `.rs` files in workspace path dependencies.
///
/// Runs `cargo metadata` and walks the `src/` directory of every local
/// dependency that is not the current crate.
pub fn dependency_sources() -> Result<Vec<PathBuf>, String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .output()
        .map_err(|e| format!("running `cargo metadata`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata json: {e}"))?;
    let own = manifest_dir()
        .ok()
        .map(|d| d.join("Cargo.toml"))
        .and_then(|p| p.canonicalize().ok());
    let mut dirs = Vec::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        if !pkg["source"].is_null() {
            continue;
        }
        let Some(manifest) = pkg["manifest_path"].as_str() else {
            continue;
        };
        if own.as_deref() == Path::new(manifest).canonicalize().ok().as_deref() {
            continue;
        }
        let src = Path::new(manifest)
            .parent()
            .unwrap_or(Path::new("."))
            .join("src");
        if src.is_dir() {
            dirs.push(src);
        }
    }
    let mut files = Vec::new();
    for d in dirs {
        walk_dir(&d, &mut files);
    }
    Ok(dedup(files))
}

/// Input file lists for a tool run.
pub struct Sources {
    /// Files that may contain graph declarations.
    pub graph: Vec<String>,
    /// Files scanned only for `#[dataflow]` accesses, not declarations.
    pub scan_only: Vec<String>,
}

/// Resolve command-line file arguments into [`Sources`].
///
/// If `files` is empty, the current crate's sources are used. If `deps` is
/// true, workspace path dependencies are added as scan-only sources.
pub fn gather(files: &[String], deps: bool, tool: &str) -> Result<Sources, String> {
    let graph = if files.is_empty() {
        let found = crate_default_inputs()?;
        eprintln!(
            "{tool}: no inputs given; scanning this crate ({} files)",
            found.len()
        );
        found
    } else {
        collect_inputs(files)?
    };
    let scan_only = if deps {
        dependency_sources()?
    } else {
        Vec::new()
    };
    let strings = |v: Vec<PathBuf>| v.iter().map(|p| p.display().to_string()).collect();
    Ok(Sources {
        graph: strings(graph),
        scan_only: strings(scan_only),
    })
}

/// The result of scanning a set of input sources.
#[derive(Default)]
pub struct Scan {
    /// Parsed graph declarations.
    pub decls: Vec<Decl>,
    /// Discovered `#[dataflow]` accesses in graph sources and dependencies.
    pub scanned_accesses: Vec<Discovered>,
    /// `(module, name)` pairs for `#[dataflow]` fns in graph sources.
    pub fns: Vec<(String, String)>,
    /// `(module, name)` pairs for `#[dataflow]` fns in dependency sources.
    pub dep_fns: Vec<(String, String)>,
    /// Discovered `#[dataflow_bundle]` modules.
    pub bundles: Vec<Bundle>,
}

/// Scan all sources and return the collected declarations, accesses, and bundles.
pub fn scan(sources: &Sources) -> Result<Scan, String> {
    let mut out = Scan::default();
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
        if graph_source {
            out.decls
                .extend(crate::parse_source(&src, file).map_err(|e| e.to_string())?);
        }
        crate::scan_source(&src, file, &mut out.scanned_accesses);
        crate::scan_fns(
            &src,
            file,
            if graph_source {
                &mut out.fns
            } else {
                &mut out.dep_fns
            },
        );
        crate::scan_bundles(&src, file, &mut out.bundles);
    }
    Ok(out)
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())))
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
        std::fs::write(src.join("main.rs"), "mod a;\nmod deep;\nmod inline { }\n").unwrap();
        std::fs::write(src.join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(src.join("deep").join("mod.rs"), "mod b;\n").unwrap();
        std::fs::write(src.join("deep").join("b.rs"), "pub fn b() {}\n").unwrap();

        let root = src.join("main.rs");
        let mut out = vec![root.clone()];
        expand_modules(&root, &mut out);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(&src).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["main.rs", "a.rs", "deep/mod.rs", "deep/b.rs"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_inputs_walk_and_skip() {
        let dir = std::env::temp_dir().join(format!("svm_walk_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("x.rs"), "").unwrap();
        std::fs::write(dir.join("sub").join("y.rs"), "").unwrap();
        std::fs::write(dir.join("target").join("z.rs"), "").unwrap();
        std::fs::write(dir.join(".git").join("g.rs"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();

        let got = collect_inputs(&[dir.display().to_string()]).unwrap();
        let mut names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["x.rs", "y.rs"]);
        assert!(collect_inputs(&["nope-does-not-exist".into()]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
