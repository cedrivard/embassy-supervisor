use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use embassy_supervisor_tools::inputs::Sources;
use embassy_supervisor_tools::{
    Decl, Options, coverage_warnings, exclude, inputs, legend_diagram, model_json, out, render,
    resolve,
};

const TOOL: &str = "supervisor-mermaid";

const USAGE: &str = "\
supervisor-mermaid: Mermaid diagrams from embassy-supervisor graph declarations

USAGE:
    supervisor-mermaid [OPTIONS] [FILE|DIR]...

Reads `supervisor_graph!`, `supervisor_fragment!` and `compose_graph!` from the
given Rust sources. A directory is walked recursively for `*.rs`; with no
inputs at all, the crate the working directory is in is scanned (its `src/`
roots, expanded through `mod` declarations). Pass every file that takes part in
a graph: a compose site draws its fragments only if their declaring files are
given too. `-` reads stdin.

When several declarations are found on a terminal, a prompt asks which to
render; `--select`, `--all` or a pipe skips it.

OPTIONS:
  inputs
        --deps             also scan the workspace's path dependencies (via
                           `cargo metadata`), for graphs adopting another
                           crate's `#[dataflow]` fns

  what to draw
        --runtime          the running system: every signal and resource slot,
                           and no bring-up edges by default
        --states           node lifecycles, as a state diagram; with --signals,
                           one composite per node carrying its concrete gates
        --runtime-deps     with --runtime, restore every `deps:` edge as dotted
                           `spawn` context (overrides --anchor-uncoupled)
        --anchor-uncoupled with --runtime, restore dotted `deps:` edges that
                           touch a node with no runtime coupling or resource
    -s, --signals          draw declared and scanned dataflow as dotted signal
                           edges (the runtime view always draws solid edges)
    -f, --full-paths       label signals with the declared path, not the last
                           segment that tells them apart
        --hide-cfg         omit `#[cfg(...)]` predicates from labels and edges
    -x, --exclude <NAMES>  leave out these nodes or pools (comma separated), and
                           every edge that named them; repeatable
        --fragments        box each fragment's items in a subgraph
        --executors        box nodes by the executor they spawn through instead

  layout
    -d, --direction <DIR>  TD (default), TB, LR, RL or BT; reaches subgraphs and
                           composite states too
        --layout <ENGINE>  ask the renderer for a layout engine (`elk` is the
                           one worth asking for, on large graphs)
        --title <TEXT>     override the Mermaid and HTML page title
        --no-title         omit the Mermaid and HTML page title
        --max-fanout <N>   collapse a signal's readers into one aggregate box
                           once more than N nodes read it; 0, the default,
                           never collapses
        --h-spacing <N>    horizontal gap between boxes, in pixels (Mermaid
                           defaults to 50)
        --v-spacing <N>    vertical gap between boxes
    -l, --legend           add a key, after the graph in the layout
        --links <TPL>      add click links to each node's declaration; {file}
                           and {line} in the template are substituted

  output
    -m, --markdown         wrap each diagram in a mermaid code fence, and give
                           the legend a diagram of its own below the graph
        --select <WHICH>   render only this declaration, by name or by its
                           number in --list order
        --all              render every declaration, never prompting
        --list             list the declarations found, and stop
        --json             print the graph model as JSON instead of a diagram
                           (with every warning, under \"warnings\")
        --check            verify and stop: print the diagnostics, no diagram;
                           exit non-zero on any warning, CI's guard against
                           graphs and `#[dataflow]` fns drifting apart (what
                           the dataflow itself says is `supervisor-lint`);
                           every declaration found is checked, whatever
                           --select or -x leaves to draw
        --live-url         print a mermaid.live share link per diagram
        --html <FILE>      write a self-rendering HTML page (mermaid.js CDN)
        --render <FILE>    render through `mmdc` (svg/png/pdf, by extension)
        --update <FILE>    rewrite the managed block in a markdown file
        --watch            re-run whenever an input file changes (needs a
                           destination: -o, --html, --render or --update)
    -o, --output <FILE>    write to a file instead of stdout
    -h, --help             show this
    -V, --version          print the version
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    files: Vec<String>,
    opts: Options,
    markdown: bool,
    list: bool,
    select: Option<String>,
    all: bool,
    deps: bool,
    json: bool,
    check: bool,
    live_url: bool,
    html: Option<String>,
    render_to: Option<String>,
    update: Option<String>,
    watch: bool,
    output: Option<String>,
    exclude: Vec<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(raw_args: impl IntoIterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args {
        files: Vec::new(),
        opts: Options::default(),
        markdown: false,
        list: false,
        select: None,
        all: false,
        deps: false,
        json: false,
        check: false,
        live_url: false,
        html: None,
        render_to: None,
        update: None,
        watch: false,
        output: None,
        exclude: Vec::new(),
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
            "-V" | "--version" => {
                println!("{TOOL} {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-s" | "--signals" => args.opts.signals = true,
            "-f" | "--full-paths" => args.opts.full_paths = true,
            "--hide-cfg" => args.opts.show_cfg = false,
            "--fragments" => args.opts.fragments = true,
            "--executors" => args.opts.executors = true,
            "--states" => args.opts.states = true,
            "--runtime" => args.opts.runtime = true,
            "--runtime-deps" => args.opts.runtime_deps = true,
            "--anchor-uncoupled" => args.opts.anchor_uncoupled = true,
            "-l" | "--legend" => args.opts.legend = true,
            "-m" | "--markdown" => args.markdown = true,
            "--list" => args.list = true,
            "--all" => args.all = true,
            "--deps" => args.deps = true,
            "--json" => args.json = true,
            "--check" => args.check = true,
            "--live-url" => args.live_url = true,
            "--watch" => args.watch = true,
            "-d" | "--direction" => {
                let d = value("--direction")?.to_uppercase();
                if !matches!(d.as_str(), "TD" | "TB" | "LR" | "RL" | "BT") {
                    return Err(format!("unknown direction `{d}`; use TD, TB, LR, RL or BT"));
                }
                args.opts.direction = d;
            }
            "-x" | "--exclude" => args.exclude.extend(
                value("--exclude")?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            ),
            "--layout" => args.opts.layout = Some(value("--layout")?),
            "--title" => {
                let title = value("--title")?;
                if title.contains('\n') || title.contains('\r') {
                    return Err("`--title` must be a single line".to_string());
                }
                args.opts.title = Some(title);
            }
            "--no-title" => args.opts.show_title = false,
            "--max-fanout" => {
                args.opts.max_fanout = value("--max-fanout")?
                    .parse::<usize>()
                    .map_err(|_| "`--max-fanout` needs a number of readers".to_string())?;
            }
            "--links" => args.opts.links = Some(value("--links")?),
            "--h-spacing" => args.opts.h_spacing = Some(spacing(&value("--h-spacing")?)?),
            "--v-spacing" => args.opts.v_spacing = Some(spacing(&value("--v-spacing")?)?),
            "--select" => args.select = Some(value("--select")?),
            "--html" => args.html = Some(value("--html")?),
            "--render" => args.render_to = Some(value("--render")?),
            "--update" => args.update = Some(value("--update")?),
            "-o" | "--output" => args.output = Some(value("--output")?),
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option `{other}`; see --help"));
            }
            other => args.files.push(other.to_string()),
        }
    }
    if args.opts.states && args.opts.runtime {
        return Err("`--states` and `--runtime` are different diagrams; pick one".to_string());
    }
    if args.opts.anchor_uncoupled && !args.opts.runtime {
        return Err(
            "`--anchor-uncoupled` is a runtime-view option; the bring-up chart draws \
             every dep edge already"
                .to_string(),
        );
    }
    if args.opts.runtime_deps && !args.opts.runtime {
        return Err(
            "`--runtime-deps` is a runtime-view option; the bring-up chart draws \
             every dep edge already"
                .to_string(),
        );
    }
    if args.opts.fragments && args.opts.executors {
        return Err(
            "`--fragments` and `--executors` are two groupings of the same boxes; pick one"
                .to_string(),
        );
    }
    if args.opts.title.is_some() && !args.opts.show_title {
        return Err("`--title` and `--no-title` cannot be used together".to_string());
    }
    if args.watch
        && args.output.is_none()
        && args.html.is_none()
        && args.render_to.is_none()
        && args.update.is_none()
    {
        return Err(
            "`--watch` re-runs into a destination; give it -o, --html, --render or --update"
                .to_string(),
        );
    }
    Ok(Some(args))
}

fn spacing(v: &str) -> Result<u32, String> {
    match v.parse::<u32>() {
        Ok(0) | Err(_) => Err(format!(
            "`{v}` is not a spacing; give a whole number of pixels"
        )),
        Ok(n) => Ok(n),
    }
}

/// What this combination accepts and never uses, one note per flag (`-o`,
/// the `--states` view flags, and the output flags a `--list`/`--check`/`--json`
/// run returns before).
fn ignored_flags(args: &Args) -> Vec<String> {
    let mut notes = Vec::new();
    let to_output = args.json
        || args.live_url
        || (args.html.is_none() && args.render_to.is_none() && args.update.is_none());
    if args.output.is_some() && (args.list || args.check || !to_output) {
        notes.push(
            "`-o` is ignored: nothing in this run writes to it (a diagram on stdout, \
             `--json` and `--live-url` do)"
                .to_string(),
        );
    }
    if args.opts.states {
        let o = &args.opts;
        for (given, flag) in [
            (o.links.is_some(), "--links"),
            (o.fragments, "--fragments"),
            (o.executors, "--executors"),
            (o.max_fanout > 0, "--max-fanout"),
            (o.full_paths, "--full-paths"),
            (!o.show_cfg, "--hide-cfg"),
        ] {
            if given {
                notes.push(format!(
                    "`{flag}` does nothing on `--states`, a lifecycle diagram"
                ));
            }
        }
    }
    let stops_early = if args.list {
        Some("--list")
    } else if args.check {
        Some("--check")
    } else if args.json {
        Some("--json")
    } else {
        None
    };
    if let Some(mode) = stops_early {
        for (given, flag) in [
            (args.live_url, "--live-url"),
            (args.html.is_some(), "--html"),
            (args.render_to.is_some(), "--render"),
            (args.update.is_some(), "--update"),
            (args.markdown, "--markdown"),
        ] {
            if given {
                notes.push(format!("`{flag}` is ignored: `{mode}` stops before it"));
            }
        }
    }
    notes
}

fn run() -> Result<(), String> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    for note in ignored_flags(&args) {
        eprintln!("{TOOL}: warning: {note}");
    }
    let files = inputs::gather(&args.files, args.deps, TOOL)?;
    if args.watch {
        watch(&args, &files)
    } else {
        generate(&args, &files)
    }
}

fn watch(args: &Args, files: &Sources) -> Result<(), String> {
    let all: Vec<&String> = files.graph.iter().chain(&files.scan_only).collect();
    if all.iter().any(|f| *f == "-") {
        return Err("`--watch` cannot watch stdin".to_string());
    }
    let stamp = |f: &&String| std::fs::metadata(f).and_then(|m| m.modified()).ok();
    loop {
        let seen: Vec<_> = all.iter().map(stamp).collect();
        match generate(args, files) {
            Ok(()) => eprintln!("{TOOL}: rendered; watching {} files", all.len()),
            Err(e) => eprintln!("{TOOL}: {e}"),
        }
        while all.iter().map(stamp).collect::<Vec<_>>() == seen {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
}

fn generate(args: &Args, files: &Sources) -> Result<(), String> {
    let scan = inputs::scan(files)?;
    let (decls, discovered) = (scan.decls, scan.scanned_accesses);
    let (scanned, dep_scanned, bundles) = (scan.fns, scan.dep_fns, scan.bundles);
    let mut warned = 0usize;
    if decls.is_empty() {
        return Err("no graph declaration found in the given files".to_string());
    }

    let (mut resolved, warnings) = resolve(decls);
    // Over every declaration, before -x or a selection narrows what is drawn:
    // whether a fn is bound is a fact of the sources, not of the view.
    let coverage = coverage_warnings(&resolved, &scanned, &dep_scanned, &bundles);
    let mut all_warnings = warnings.clone();
    for w in &warnings {
        eprintln!("{TOOL}: warning: {w}");
        warned += 1;
    }

    if !args.exclude.is_empty() {
        let mut unmatched = args.exclude.clone();
        for decl in &mut resolved {
            let missed = exclude(decl, &args.exclude);
            unmatched.retain(|n| missed.contains(n));
        }
        if !unmatched.is_empty() {
            return Err(format!(
                "no node or pool named {} in this graph",
                unmatched.join(", ")
            ));
        }
    }

    if args.list {
        for (i, d) in resolved.iter().enumerate() {
            println!("[{}] {}", i + 1, describe(d));
        }
        return Ok(());
    }

    if let Some(want) = &args.select {
        match want.parse::<usize>() {
            Ok(n) if (1..=resolved.len()).contains(&n) => {
                resolved = vec![resolved.swap_remove(n - 1)];
            }
            Ok(n) => {
                return Err(format!(
                    "`--select {n}` is out of range; --list shows 1..{}",
                    resolved.len()
                ));
            }
            Err(_) => {
                resolved.retain(|d| d.name().as_deref() == Some(want.as_str()));
                if resolved.is_empty() {
                    return Err(format!("no declaration named `{want}`"));
                }
            }
        }
    } else if resolved.len() > 1 && !args.all && !args.json && !args.watch && !args.check {
        resolved = pick(resolved, &files.graph)?;
    }

    for w in coverage {
        eprintln!("{TOOL}: warning: {w}");
        all_warnings.push(w);
        warned += 1;
    }
    if args.check {
        if warned > 0 {
            return Err(format!(
                "--check: {} — the graphs and the `#[dataflow]` fns disagree; see above",
                match warned {
                    1 => "1 warning".to_string(),
                    n => format!("{n} warnings"),
                }
            ));
        }
        eprintln!(
            "{TOOL}: --check: OK — {} declaration{}, graphs and `#[dataflow]` fns agree",
            resolved.len(),
            if resolved.len() == 1 { "" } else { "s" },
        );
        return Ok(());
    }

    if args.json {
        let mut model = model_json(&resolved, &discovered);
        model["warnings"] = serde_json::json!(all_warnings);
        let model = serde_json::to_string_pretty(&model).map_err(|e| e.to_string())?;
        return write_out(args, &format!("{model}\n"));
    }

    let apart = (args.markdown || args.update.is_some()) && args.opts.legend;
    let legend_opts = args.opts.clone();
    let mut opts = args.opts.clone();
    opts.discovered = discovered;
    opts.bundles = bundles;
    opts.legend = opts.legend && !apart;

    let mut diagrams: Vec<(String, String)> = resolved
        .iter()
        .map(|d| (describe(d), render(d, &opts)))
        .collect();
    if apart {
        diagrams.push(("legend".to_string(), legend_diagram(&legend_opts)));
    }
    // `--render` draws one image, so it is checked before anything is written,
    // and a legend the text outputs detach is chained back into it.
    let chained = match &args.render_to {
        Some(_) => render_image(&resolved, &opts, apart)?,
        None => None,
    };

    let mut handled = false;
    if args.live_url {
        let mut lines = String::new();
        for (heading, code) in &diagrams {
            if diagrams.len() > 1 {
                lines.push_str(&format!("{heading}\n  {}\n", out::live_url(code)));
            } else {
                lines.push_str(&format!("{}\n", out::live_url(code)));
            }
        }
        write_out(args, &lines)?;
        handled = true;
    }
    if let Some(path) = &args.html {
        std::fs::write(path, out::html_page(html_page_title(&args.opts), &diagrams))
            .map_err(|e| format!("{path}: {e}"))?;
        handled = true;
    }
    if let Some(path) = &args.render_to {
        out::mmdc_render(chained.as_deref().unwrap_or(&diagrams[0].1), path)?;
        handled = true;
    }
    if let Some(path) = &args.update {
        out::update_markdown(path, &fenced(&diagrams))?;
        handled = true;
    }
    if handled {
        return Ok(());
    }

    let text = if args.markdown {
        fenced(&diagrams)
    } else {
        diagrams
            .iter()
            .map(|(_, code)| code.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    write_out(args, &text)
}

fn html_page_title(opts: &Options) -> Option<&str> {
    opts.show_title
        .then(|| opts.title.as_deref().unwrap_or("supervisor graphs"))
}

/// `--render`'s one image: several declarations are refused; with a detached
/// legend, the declaration drawn again with the legend chained in (`None`:
/// the text diagram is the image).
fn render_image(resolved: &[Decl], opts: &Options, apart: bool) -> Result<Option<String>, String> {
    match resolved {
        [one] => Ok(apart.then(|| {
            render(
                one,
                &Options {
                    legend: true,
                    ..opts.clone()
                },
            )
        })),
        many => Err(format!(
            "`--render` writes one image and there are {} declarations; --select one",
            many.len()
        )),
    }
}

fn describe(d: &Decl) -> String {
    format!(
        "{} {}  ({}:{}, {} items)",
        d.kind.macro_name(),
        d.name().unwrap_or_else(|| "<unnamed>".to_string()),
        d.origin,
        d.line,
        d.spec.items.len(),
    )
}

fn pick(resolved: Vec<Decl>, files: &[String]) -> Result<Vec<Decl>, String> {
    if !std::io::stdin().is_terminal()
        || !std::io::stderr().is_terminal()
        || files.iter().any(|f| f == "-")
    {
        return Ok(resolved);
    }
    eprintln!("{} declarations found:", resolved.len());
    for (i, d) in resolved.iter().enumerate() {
        eprintln!("  [{}] {}", i + 1, describe(d));
    }
    eprint!("render which? [1-{}, or `a` for all] ", resolved.len());
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("reading the answer: {e}"))?;
    let answer = line.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("a") {
        return Ok(resolved);
    }
    match answer.parse::<usize>() {
        Ok(n) if (1..=resolved.len()).contains(&n) => {
            let mut resolved = resolved;
            Ok(vec![resolved.swap_remove(n - 1)])
        }
        _ => Err(format!("`{answer}` is not one of the listed numbers")),
    }
}

fn fenced(diagrams: &[(String, String)]) -> String {
    let mut s = String::new();
    for (i, (heading, code)) in diagrams.iter().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        if diagrams.len() > 1 {
            s.push_str(&format!("### {heading}\n\n"));
        }
        s.push_str("```mermaid\n");
        s.push_str(code);
        s.push_str("```\n");
    }
    s
}

fn write_out(args: &Args, text: &str) -> Result<(), String> {
    match &args.output {
        Some(path) => std::fs::write(path, text).map_err(|e| format!("{path}: {e}")),
        None => std::io::stdout()
            .write_all(text.as_bytes())
            .map_err(|e| format!("writing to stdout: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Options, Sources, USAGE, generate, html_page_title, ignored_flags, parse_args_from,
        render_image,
    };

    fn parse(args: &[&str]) -> Result<super::Args, String> {
        parse_args_from(args.iter().map(|arg| (*arg).to_string()))
            .map(|args| args.expect("test arguments do not ask for help"))
    }

    #[test]
    fn lint_flags_belong_to_the_other_binary() {
        for flag in ["--lint", "--allow"] {
            let err = parse(&[flag, "all"])
                .err()
                .expect("no longer this tool's option");
            assert!(err.contains(flag), "{err}");
        }
    }

    #[test]
    fn title_options_parse_and_validate_their_combination() {
        let titled = parse(&["--title", "Firmware bring-up"]).unwrap();
        assert_eq!(titled.opts.title.as_deref(), Some("Firmware bring-up"));
        assert!(titled.opts.show_title);

        let untitled = parse(&["--no-title"]).unwrap();
        assert!(untitled.opts.title.is_none());
        assert!(!untitled.opts.show_title);

        let conflict = parse(&["--title", "Firmware bring-up", "--no-title"]);
        assert_eq!(
            conflict.as_ref().err().map(String::as_str),
            Some("`--title` and `--no-title` cannot be used together")
        );

        let multiline = parse(&["--title", "first\nsecond"]);
        assert_eq!(
            multiline.as_ref().err().map(String::as_str),
            Some("`--title` must be a single line")
        );
    }

    #[test]
    fn html_page_title_follows_title_options() {
        let default = parse(&[]).unwrap();
        assert_eq!(html_page_title(&default.opts), Some("supervisor graphs"));

        let titled = parse(&["--title", "Firmware bring-up"]).unwrap();
        assert_eq!(html_page_title(&titled.opts), Some("Firmware bring-up"));

        let untitled = parse(&["--no-title"]).unwrap();
        assert_eq!(html_page_title(&untitled.opts), None);
    }

    #[test]
    fn runtime_deps_is_runtime_only_and_supersedes_anchor_mode() {
        let full = parse(&["--runtime", "--runtime-deps", "--anchor-uncoupled"]).unwrap();
        assert!(full.opts.runtime_deps);
        assert!(full.opts.anchor_uncoupled);

        let standalone = parse(&["--runtime-deps"]);
        assert_eq!(
            standalone.as_ref().err().map(String::as_str),
            Some(
                "`--runtime-deps` is a runtime-view option; the bring-up chart draws every dep edge already"
            )
        );
    }

    #[test]
    fn every_flag_the_parser_takes_is_in_the_help() {
        let src = include_str!("mermaid.rs");
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
        assert!(checked > 10, "found only {checked} flags; the scan broke");
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
    fn version_prints_and_stops() {
        assert!(
            parse_args_from(["--version".to_string()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn check_covers_every_declaration_whatever_select_or_exclude_narrows() {
        let dir = std::env::temp_dir().join(format!("svm_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("two.rs");
        std::fs::write(
            &file,
            r#"
            embassy_supervisor::supervisor_graph! { name: A; node X = Terminate, task: fa, discover; }
            embassy_supervisor::supervisor_graph! { name: B; node Y = Terminate, task: fb, discover; }
            #[embassy_supervisor::dataflow]
            async fn fa(node: &'static TaskNode) { node.put(&crate::S, 1); }
            #[embassy_supervisor::dataflow]
            async fn fb(node: &'static TaskNode) { let _ = node.get(&crate::S); }
        "#,
        )
        .unwrap();
        let files = Sources {
            graph: vec![file.display().to_string()],
            scan_only: Vec::new(),
        };
        for flags in [
            &["--check"][..],
            &["--check", "--select", "A"],
            &["--check", "-x", "Y"],
        ] {
            assert_eq!(
                generate(&parse(flags).unwrap(), &files),
                Ok(()),
                "{flags:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_takes_one_declaration_and_keeps_its_legend() {
        let one = embassy_supervisor_tools::parse_source(
            "embassy_supervisor::supervisor_graph! { node A = Terminate, task: f; }",
            "t.rs",
        )
        .unwrap();
        let Ok(Some(chained)) = render_image(&one, &Options::default(), true) else {
            panic!("a detached legend is chained back into the image");
        };
        assert!(chained.contains("subgraph legend"), "{chained}");
        assert_eq!(render_image(&one, &Options::default(), false), Ok(None));

        let two = embassy_supervisor_tools::parse_source(
            "embassy_supervisor::supervisor_graph! { name: A; node X = Terminate, task: f; }\n\
             embassy_supervisor::supervisor_graph! { name: B; node Y = Terminate, task: g; }",
            "t.rs",
        )
        .unwrap();
        assert_eq!(
            render_image(&two, &Options::default(), false),
            Err(
                "`--render` writes one image and there are 2 declarations; --select one"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_flag_the_run_never_uses_is_named() {
        let notes = |a: &[&str]| ignored_flags(&parse(a).unwrap());
        let html = notes(&["--html", "g.html", "-o", "g.mmd"]);
        assert_eq!(html.len(), 1, "{html:?}");
        assert!(html[0].contains("`-o` is ignored"), "{html:?}");
        assert!(notes(&["--live-url", "--html", "g.html", "-o", "l.txt"]).is_empty());
        assert!(notes(&["--json", "-o", "m.json"]).is_empty());
        assert_eq!(notes(&["--check", "-o", "x"]).len(), 1);
        let json = notes(&["--json", "--html", "g.html"]);
        assert_eq!(json.len(), 1, "{json:?}");
        assert!(json[0].contains("`--html` is ignored"), "{json:?}");
        assert_eq!(
            notes(&["--list", "--render", "g.svg", "--update", "r.md"]).len(),
            2
        );
        let states = notes(&[
            "--states",
            "--executors",
            "--max-fanout",
            "3",
            "--links",
            "x{line}",
        ]);
        assert_eq!(states.len(), 3, "{states:?}");
        assert!(
            states.iter().all(|n| n.contains("on `--states`")),
            "{states:?}"
        );
        assert!(notes(&["--states", "--max-fanout", "0"]).is_empty());
        assert!(notes(&["--executors", "--max-fanout", "3"]).is_empty());
    }
}
