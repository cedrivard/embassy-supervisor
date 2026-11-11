use std::process::ExitCode;

use embassy_supervisor_tools::inputs::Sources;
use embassy_supervisor_tools::{
    LintCats, coverage_warnings, dataflow_lints, gate_lints, inputs, resolve,
};

const TOOL: &str = "supervisor-lint";

const USAGE: &str = "\
supervisor-lint: one-sided signals and exposed gates in an embassy-supervisor graph

USAGE:
    supervisor-lint [OPTIONS] [FILE|DIR]...

Reads the same sources `supervisor-mermaid` draws from (`supervisor_graph!`,
`supervisor_fragment!` and `compose_graph!`, plus the `#[dataflow]` fn bodies
a `discover` node or a `dataflow:` adoption binds) and reports what the
dataflow model says: a signal read where nothing writes it, a signal written
where nothing reads it, a gate-wrapped static (`Backed`, `Leased`, `VetoGate`)
reachable from outside its module. The static shape of the diagnostics a
running supervisor logs, at build time instead of on a serial console.

A directory is walked recursively for `*.rs`; with no inputs at all, the crate
the working directory is in is scanned (its `src/` roots, expanded through
`mod` declarations). `-` reads stdin. Every declaration found is linted;
`public-gate` alone needs none. `--allow` is how a known, accepted absence is
written down where it gets reviewed.

Exit status: 0 for no findings, 1 for findings (a stale `--allow` included)
or an input file that does not parse, 2 when the run itself fails (a bad
option, an unreadable input, no graph).

OPTIONS:
        --deps             also scan the workspace's path dependencies (via
                           `cargo metadata`), for graphs adopting another
                           crate's `#[dataflow]` fns
        --only <CATS>      restrict to these categories (comma separated,
                           repeatable): `orphan-reads` (read, never written),
                           `dead-writes` (written, never read; `observed` /
                           `beat` entries and `beat_*` verbs are exempt, their
                           consumer is the supervisor), `public-gate` (a gated
                           static any module can reach around its gate), or
                           `all`, the default
        --allow <SIGNALS>  accept these signals' findings (comma separated,
                           repeatable; matched like signal labels, by
                           `::`-suffix); an entry suppressing nothing is
                           reported whenever `--only` keeps a signal category
    -h, --help             show this
    -V, --version          print the version
";

fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            ExitCode::from(2)
        }
    }
}

struct Args {
    files: Vec<String>,
    deps: bool,
    only: LintCats,
    allow: Vec<String>,
}

fn parse_args_from(raw_args: impl IntoIterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args {
        files: Vec::new(),
        deps: false,
        only: LintCats::default(),
        allow: Vec::new(),
    };
    let mut it = raw_args.into_iter();
    while let Some(raw) = it.next() {
        // `--opt=value` carries its value inline; that is also how a value
        // starting with `-` is written.
        let (arg, mut inline) = match raw.split_once('=') {
            Some((opt, v)) if opt.starts_with("--") => (opt.to_string(), Some(v.to_string())),
            _ => (raw, None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = inline.take() {
                return Ok(v);
            }
            match it.next() {
                Some(next) if next.starts_with('-') && next != "-" => {
                    Err(format!("`{what}` needs a value, found `{next}`"))
                }
                Some(v) => Ok(v),
                None => Err(format!("`{what}` needs a value; see --help")),
            }
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("{TOOL} {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--deps" => args.deps = true,
            "--only" => args.only.parse(&value("--only")?)?,
            "--allow" => args.allow.extend(
                value("--allow")?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            ),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`; see --help"));
            }
            other => args.files.push(other.to_string()),
        }
        if inline.is_some() {
            return Err(format!("`{arg}` takes no value; see --help"));
        }
    }
    if !args.only.any() {
        args.only = LintCats::all();
    }
    Ok(Some(args))
}

/// A note for `--allow` given where no category could use it.
fn ignored_allow(args: &Args) -> Option<&'static str> {
    (!args.allow.is_empty() && !args.only.dataflow()).then_some(
        "`--allow` is ignored: it names signals, and only `orphan-reads` and `dead-writes` lint signals",
    )
}

fn run() -> Result<usize, String> {
    let Some(args) = parse_args_from(std::env::args().skip(1))? else {
        return Ok(0);
    };
    if let Some(note) = ignored_allow(&args) {
        eprintln!("{TOOL}: warning: {note}");
    }
    let files = inputs::gather(&args.files, args.deps)?;
    if args.files.is_empty() {
        eprintln!(
            "{TOOL}: no inputs given; scanning this crate ({} files)",
            files.graph.len()
        );
    }
    lint(&args, &files, &mut std::io::stderr())
}

/// Lint `files`, reporting to `err` (stderr outside tests), and return the
/// number of findings plus the inputs that could not be scanned in full.
fn lint(args: &Args, files: &Sources, err: &mut dyn std::io::Write) -> Result<usize, String> {
    let scan = inputs::scan(files)?;
    // A file that does not parse hides its accesses and gates: the lint's
    // silence about it would be a false all-clear, so it fails the run.
    let unscanned = scan.warnings.len();
    for w in &scan.warnings {
        let _ = writeln!(err, "{TOOL}: warning: {w}");
    }
    if args.only.dataflow() && scan.decls.is_empty() {
        return Err("no graph declaration found in the given files".to_string());
    }
    let (resolved, warnings) = resolve(scan.decls);
    // Drift explains a signal that looks one-sided; with no signal category
    // there is none to explain, and a graph-less run binds nothing.
    let drift = if args.only.dataflow() {
        coverage_warnings(&resolved, &scan.fns, &scan.dep_fns, &scan.bundles)
    } else {
        Vec::new()
    };

    let mut warned = 0usize;
    for w in warnings.into_iter().chain(drift) {
        let _ = writeln!(err, "{TOOL}: warning: {w}");
        warned += 1;
    }

    let mut findings = 0usize;
    let dataflow = dataflow_lints(
        &resolved,
        &scan.scanned_accesses,
        &scan.bundles,
        &args.only,
        &args.allow,
    );
    for w in dataflow
        .iter()
        .chain(&gate_lints(&scan.gate_statics, &args.only))
    {
        let _ = writeln!(err, "{TOOL}: lint: {w}");
        findings += 1;
    }

    let _ = writeln!(
        err,
        "{TOOL}: {} declaration{}, {}{}{}",
        resolved.len(),
        if resolved.len() == 1 { "" } else { "s" },
        match findings {
            0 => "no findings".to_string(),
            1 => "1 finding".to_string(),
            n => format!("{n} findings"),
        },
        match unscanned {
            0 => String::new(),
            n => format!(
                ", {n} input{} not scanned in full",
                if n == 1 { "" } else { "s" }
            ),
        },
        match warned {
            0 => String::new(),
            n => format!(
                " ({n} warning{}: the model may be short — `supervisor-mermaid --check` \
                 is where those fail)",
                if n == 1 { "" } else { "s" }
            ),
        }
    );
    Ok(findings + unscanned)
}

#[cfg(test)]
mod tests {
    use super::{Sources, USAGE, ignored_allow, lint, parse_args_from};

    fn parse(args: &[&str]) -> Result<super::Args, String> {
        parse_args_from(args.iter().map(|arg| (*arg).to_string()))
            .map(|args| args.expect("test arguments do not ask for help"))
    }

    #[test]
    fn no_category_flag_lints_everything_and_only_narrows_it() {
        let all = parse(&["src"]).unwrap();
        assert!(all.only.orphan_reads && all.only.dead_writes);
        assert_eq!(all.files, vec!["src"]);

        let narrowed = parse(&["--only", "dead-writes", "src"]).unwrap();
        assert!(narrowed.only.dead_writes && !narrowed.only.orphan_reads);
        assert_eq!(narrowed.files, vec!["src"]);

        let repeated = parse(&["--only", "dead-writes", "--only", "orphan-reads"]).unwrap();
        assert!(repeated.only.dead_writes && repeated.only.orphan_reads);

        let bogus = parse(&["--only", "bogus-category"]).err();
        assert!(
            bogus
                .as_deref()
                .is_some_and(|e| e.contains("unknown lint category")),
            "{bogus:?}"
        );
    }

    #[test]
    fn allow_takes_a_comma_separated_list_and_repeats() {
        let args = parse(&["--allow", "ESTIMATE, TAP", "--allow", "OUT"]).unwrap();
        assert_eq!(args.allow, ["ESTIMATE", "TAP", "OUT"]);
        assert!(parse(&["--allow"]).is_err(), "`--allow` needs a value");
    }

    #[test]
    fn every_flag_the_parser_takes_is_in_the_help() {
        let src = include_str!("lint.rs");
        let body = &src[src.find("match arg.as_str() {").unwrap()
            ..src.find("other => args.files.push").unwrap()];
        let mut checked = 0;
        for literal in body.split('"').skip(1).step_by(2) {
            if literal.starts_with('-') && literal.len() > 1 {
                assert!(
                    USAGE.contains(literal),
                    "`{literal}` is accepted but missing from --help"
                );
                checked += 1;
            }
        }
        assert!(checked > 3, "found only {checked} flags; the scan broke");
    }

    #[test]
    fn the_readme_carries_the_help_text_verbatim() {
        assert!(
            include_str!("../../README.md").contains(USAGE),
            "README.md must quote `--help` verbatim"
        );
    }

    #[test]
    fn every_help_line_lines_up() {
        let options = USAGE
            .split_once("OPTIONS:\n")
            .expect("an OPTIONS section")
            .1;
        for line in options.lines() {
            let b = line.as_bytes();
            let at_27 = b.get(26) == Some(&b' ') && b.get(27).is_some_and(|c| *c != b' ');
            let flag = (line.starts_with("    -") && !line.starts_with("    --"))
                || line.starts_with("        --");
            let continued = line.starts_with(&" ".repeat(27));
            let heading = line.starts_with("  ") && !line.starts_with("   ");
            assert!(
                line.is_empty() || heading || ((flag || continued) && at_27),
                "`{line}` does not line up with column 28"
            );
        }
    }

    #[test]
    fn an_option_value_is_never_another_option() {
        let err = parse(&["--allow", "--deps"]).err();
        assert_eq!(
            err.as_deref(),
            Some("`--allow` needs a value, found `--deps`")
        );
        let inline = parse(&["--only=dead-writes", "--allow=-ODD"]).unwrap();
        assert!(inline.only.dead_writes && !inline.only.orphan_reads);
        assert_eq!(inline.allow, ["-ODD"]);
        assert_eq!(
            parse(&["--deps=1"]).err().as_deref(),
            Some("`--deps` takes no value; see --help")
        );
    }

    #[test]
    fn version_prints_and_stops() {
        assert!(
            parse_args_from(["--version".to_string()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn allow_without_a_signal_category_is_named() {
        let gate_only = parse(&["--only", "public-gate", "--allow", "X"]).unwrap();
        assert!(ignored_allow(&gate_only).is_some());
        assert!(ignored_allow(&parse(&["--allow", "X"]).unwrap()).is_none());
    }

    #[test]
    fn public_gate_alone_lints_a_file_with_no_graph() {
        let dir = std::env::temp_dir().join(format!("svl_gate_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gate.rs");
        std::fs::write(
            &path,
            "pub static EST: Backed<u32> = Backed::new(0);\n\
             #[embassy_supervisor::dataflow]\n\
             fn est(node: &'static TaskNode) -> u32 { node.get(&EST) }\n",
        )
        .unwrap();
        let mut files = Sources {
            graph: vec![path.display().to_string()],
            ..Sources::default()
        };
        let run = |only: &str, files: &Sources| {
            let mut err = Vec::new();
            let found = lint(&parse(&["--only", only]).unwrap(), files, &mut err);
            (found, String::from_utf8(err).unwrap())
        };
        let (found, err) = run("public-gate", &files);
        assert_eq!(found, Ok(1), "{err}");
        assert!(err.contains("lint: public gate: "), "{err}");
        assert!(
            !err.contains("warning"),
            "no signal category, no drift: {err}"
        );
        assert_eq!(
            run("dead-writes", &files).0,
            Err("no graph declaration found in the given files".to_string())
        );

        let graph = dir.join("graph.rs");
        std::fs::write(
            &graph,
            "embassy_supervisor::supervisor_graph! { node A = Terminate, task: a; }\n",
        )
        .unwrap();
        files.graph.push(graph.display().to_string());
        let (_, err) = run("public-gate", &files);
        assert!(!err.contains("is not bound"), "{err}");
        let (_, err) = run("dead-writes", &files);
        assert!(
            err.contains("warning: `#[dataflow]` fn `gate::est` is not bound"),
            "a signal category reports drift: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
