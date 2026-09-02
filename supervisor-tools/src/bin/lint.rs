use std::process::ExitCode;

use embassy_supervisor_tools::inputs::Sources;
use embassy_supervisor_tools::{
    LintCats, coverage_warnings, dataflow_lints, gate_lints, inputs, resolve,
};

const TOOL: &str = "supervisor-lint";

const USAGE: &str = "\
supervisor-lint — one-sided signals and exposed gates in an embassy-supervisor graph

USAGE:
    supervisor-lint [OPTIONS] [FILE|DIR]...

Reads the same sources `supervisor-mermaid` draws from — `supervisor_graph!`,
`supervisor_fragment!` and `compose_graph!`, plus the `#[dataflow]` fn bodies
a `discover` node or a `dataflow:` adoption binds — and reports what the
dataflow model says: a signal read where nothing writes it, a signal written
where nothing reads it, a gate-wrapped static (`Backed`, `Leased`, `VetoGate`)
reachable from outside its module. The static shape of the diagnostics a
running supervisor logs, at build time instead of on a serial console.

A directory is walked recursively for `*.rs`; with no inputs at all, the crate
the working directory is in is scanned (its `src/` roots, expanded through
`mod` declarations). `-` reads stdin. Every declaration found is linted, and a
finding exits non-zero: `--allow` is how a known, accepted absence is written
down where it gets reviewed.

OPTIONS:
        --deps             also scan the workspace's path dependencies (via
                           `cargo metadata`) — for graphs adopting another
                           crate's `#[dataflow]` fns
        --only <CATS>      restrict to these categories (comma separated,
                           repeatable): `orphan-reads` (read, never written),
                           `dead-writes` (written, never read — `observed` /
                           `beat` entries and `beat_*` verbs are exempt, their
                           consumer is the supervisor), `public-gate` (a gated
                           static any module can reach around its gate), or
                           `all`, the default
        --allow <SIGNALS>  accept these signals' findings (comma separated,
                           repeatable; matched like signal labels, by
                           `::`-suffix); an entry suppressing nothing is
                           itself reported
    -h, --help             show this
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            ExitCode::FAILURE
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
    while let Some(arg) = it.next() {
        let mut value = |what: &str| -> Result<String, String> {
            it.next()
                .ok_or_else(|| format!("`{what}` needs a value; see --help"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
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
    }
    if !args.only.any() {
        args.only = LintCats::all();
    }
    Ok(Some(args))
}

fn run() -> Result<ExitCode, String> {
    let Some(args) = parse_args_from(std::env::args().skip(1))? else {
        return Ok(ExitCode::SUCCESS);
    };
    let files = inputs::gather(&args.files, args.deps, TOOL)?;
    lint(&args, &files)
}

fn lint(args: &Args, files: &Sources) -> Result<ExitCode, String> {
    let scan = inputs::scan(files)?;
    if scan.decls.is_empty() {
        return Err("no graph declaration found in the given files".to_string());
    }
    let (resolved, warnings) = resolve(scan.decls);

    let mut warned = 0usize;
    for w in warnings.into_iter().chain(coverage_warnings(
        &resolved,
        &scan.fns,
        &scan.dep_fns,
        &scan.bundles,
    )) {
        eprintln!("{TOOL}: warning: {w}");
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
        eprintln!("{TOOL}: lint: {w}");
        findings += 1;
    }

    eprintln!(
        "{TOOL}: {} declaration{}, {}{}",
        resolved.len(),
        if resolved.len() == 1 { "" } else { "s" },
        match findings {
            0 => "no findings".to_string(),
            1 => "1 finding".to_string(),
            n => format!("{n} findings"),
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
    Ok(if findings > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::{USAGE, parse_args_from};

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
    fn every_flag_is_in_the_readme_option_list() {
        let readme = include_str!("../../README.md");
        let list = readme
            .split_once("## supervisor-lint")
            .and_then(|(_, section)| section.split_once("OPTIONS:"))
            .and_then(|(_, rest)| rest.split_once("```"))
            .map(|(block, _)| block)
            .expect("the README's supervisor-lint option list");
        for line in USAGE.lines() {
            let Some(flag) = line.split_whitespace().find(|w| w.starts_with("--")) else {
                continue;
            };
            let flag = flag.trim_end_matches(',');
            if flag == "--help" {
                continue;
            }
            assert!(
                list.contains(flag),
                "`{flag}` is missing from the README list"
            );
        }
    }
}
