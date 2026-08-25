[![crates.io](https://img.shields.io/crates/v/embassy-supervisor-tools.svg)](https://crates.io/crates/embassy-supervisor-tools)
[![docs](https://img.shields.io/badge/docs-embassy--supervisor.github.io-blue)](https://embassy-supervisor.github.io/)


# embassy-supervisor-tools

Two host tools that read an `embassy-supervisor` task graph straight out of its
Rust source:

- **`supervisor-mermaid`** renders the declaration as a [Mermaid](https://mermaid.js.org) diagram;
- **`supervisor-lint`** reports the graph's one-sided signals: read by a node
  that nothing writes, written by a node that nothing reads.

Both parse the same declarations and the same `#[dataflow]` fn bodies, so a
lint finding names the same box a diagram draws.

## Install

```console
$ cargo install --path supervisor-tools --target $(rustc --print=host-tuple)
```

## Quickstart

From the crate whose graph you want drawn:

```console
$ cd firmware && supervisor-mermaid
```

No arguments means "this crate": the `src/` roots named by the nearest
`Cargo.toml`, followed through their `mod` declarations. The diagram goes to
stdout; `-o graph.mmd` writes a file, `--live-url` prints a
[mermaid.live](https://mermaid.live) link that opens with nothing installed,
`--html graph.html` writes a page that renders itself in a browser.

To point at things explicitly:

```console
$ supervisor-mermaid firmware/src/            # a directory, walked recursively
$ supervisor-mermaid src/main.rs src/tasks.rs # exact files
$ supervisor-mermaid --deps src/main.rs       # + the workspace's path deps
```

If several declarations turn up on a terminal, a prompt asks which one to
render. Answer it from the command line with `--select NAME` (or `--select 2`,
the `--list` number, the only handle an unnamed graph has) or `--all`.

## The three diagrams

**Bring-up** (the default) answers what starts after what. A `deps:` edge
joins two boxes and its weight says how much is enforced: a plain arrow is
spawn order alone, `ready` awaits `set_ready()`, a thick `ready bound` edge
propagates readiness both ways. Dotted `resources:` slots are drawn too: an
unfilled one fails the spawn, so they gate bring-up as much as `deps:` does.
Add `-s`/`--signals` for the declared and scanned dataflow as dotted signal
edges.

**Runtime** (`--runtime`) answers what the running system looks like: every
signal and resource, and no bring-up edges by default. Coupling always routes
through a signal box, so it can never be mistaken for a dep. The two relations
are genuinely different: `deps:` holds for an instant at spawn;
`reads:`/`writes:` holds for the life of the program and may legitimately
contain cycles.

- `--runtime-deps` restores every `deps:` edge as dotted `spawn` context. It
  overrides `--anchor-uncoupled` when both are given.
- `--anchor-uncoupled` restores a dotted `spawn` edge only when either
  endpoint has no runtime coupling: no declared signal, no scanned
  discovered/adopted dataflow access, no resource. A root with no `deps:` of
  its own is anchored by its dependent's edge. An edge between two
  runtime-coupled items is never restored.

**Lifecycles** (`--states`) answers what happens to one node over its life, as
a `stateDiagram-v2`. Only transitions the declaration implies are drawn: a
`bound` dep proves the cascade exists, `ready` proves the readiness gate,
`beat_timeout:` proves the liveness sweep. Nodes sharing a lifecycle shape
share one composite state. Add `-s`/`--signals` for one composite per node,
its transitions carrying the concrete gates (the slots its spawn takes, the
readiness it waits on, the slots a stop clears) and its declared reads/writes
in the title.

A `#[cfg]`-gated node, dep, signal entry or `#[dataflow]` call site draws like
any other, with the predicate in its label or on its edge: the diagram shows
every build, and the `cfg(…)` text says which. Pass `--hide-cfg` to omit those
markers.

Each diagram's Mermaid frontmatter title comes from its declaration. Use
`--title 'Firmware bring-up'` to replace it, including the generated HTML
page's browser title and heading, or `--no-title` to omit those titles.

## Output destinations

| Flag | What you get |
| --- | --- |
| `-o, --output FILE` | write the diagram to a file instead of stdout |
| `--live-url` | a `mermaid.live` share link per diagram; the diagram rides in the URL itself, nothing to install |
| `--html FILE` | one self-rendering page (mermaid.js from the CDN), a double-click view |
| `--render FILE` | an svg/png/pdf through [`mmdc`], if it is installed; the format follows the extension |
| `--update FILE.md` | rewrite the block between `<!-- supervisor-mermaid:start -->` and `<!-- supervisor-mermaid:end -->` markers in a markdown file, touching nothing else |
| `--json` | the graph model (nodes, deps, resources, provides, signals, scanned accesses) as JSON, for anything that is not a diagram |
| `--watch` | re-run whenever an input file changes; needs one of the destinations above |

`--links 'vscode://file/{file}:{line}:1'` makes each node box a link to its
declaration (`{file}` is its canonical absolute path, `{line}` its source
line). Pair it with `--html FILE` and open the page in an external browser:
VS Code's built-in Mermaid preview and GitHub disable Mermaid `click` links.

On a graph big enough to tangle: `--layout elk` asks the renderer for the ELK
engine, and `--max-fanout 6` collapses any signal read by more than six nodes
into one aggregate box naming its readers.

[`mmdc`]: https://github.com/mermaid-js/mermaid-cli

## Options

```
supervisor-mermaid — Mermaid diagrams from embassy-supervisor graph declarations

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
                           `cargo metadata`) — for graphs adopting another
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
                           once more than N nodes read it
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
                           (with every warning, under "warnings")
        --check            verify and stop: print the diagnostics, no diagram;
                           exit non-zero on any warning — CI's guard against
                           graphs and `#[dataflow]` fns drifting apart (what
                           the dataflow itself says is `supervisor-lint`)
        --live-url         print a mermaid.live share link per diagram
        --html <FILE>      write a self-rendering HTML page (mermaid.js CDN)
        --render <FILE>    render through `mmdc` (svg/png/pdf, by extension)
        --update <FILE>    rewrite the managed block in a markdown file
        --watch            re-run whenever an input file changes (needs a
                           destination: -o, --html, --render or --update)
    -o, --output <FILE>    write to a file instead of stdout
    -h, --help             show this
```

## supervisor-lint

The same model, asked what its dataflow is missing:

- `orphan-reads`: a signal some node reads that nothing in the graph writes;
- `dead-writes`: a signal some node writes that nothing reads.

The static shape of the one-sided-signal diagnostics a running supervisor
logs, at build time instead of on a serial console.

```console
$ cd firmware && supervisor-lint
$ supervisor-lint --only dead-writes --allow RATE_PID_TERMS src/
```

Every category runs unless `--only` narrows it, and a finding exits non-zero:
this is a CI gate, not a report. A one-sided signal is often a real absence of
the build (an input this target has no producer for, a telemetry tap nothing
consumes yet) rather than a mistake, so `--allow SIGNAL,…` names the accepted
ones. The list lives in the invocation, where it is reviewed like code, and an
`--allow` entry that no longer suppresses anything is itself reported.

Drift warnings (a bound fn that no scanned file defines, a scanned fn that
nothing binds) are printed here too, because that is how a signal comes to
look one-sided when it is not; failing on them is `supervisor-mermaid
--check`'s job.

### Categories and exemptions

| Category | Finding | Exempt |
| --- | --- | --- |
| `orphan-reads` | a signal is read by a node but nothing in the graph writes it | nothing |
| `dead-writes` | a signal is written by a node but nothing in the graph reads it | `observed` / `beat` entries and `beat_*` verbs: their consumer is the supervisor, so no task-side reader is needed |

`--allow` entries are matched like signal labels: exact, or by `::`-segment
suffix in either direction. An indexed element (`ARR[0]`) must be spelled with
its index: element and whole array are two coupling identities.

```
supervisor-lint — one-sided signals in an embassy-supervisor graph

USAGE:
    supervisor-lint [OPTIONS] [FILE|DIR]...

Reads the same sources `supervisor-mermaid` draws from — `supervisor_graph!`,
`supervisor_fragment!` and `compose_graph!`, plus the `#[dataflow]` fn bodies
a `discover` node or a `dataflow:` adoption binds — and reports what the
dataflow model says: a signal read where nothing writes it, a signal written
where nothing reads it. The static shape of the diagnostics a running
supervisor logs, at build time instead of on a serial console.

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
                           consumer is the supervisor), or `all`, the default
        --allow <SIGNALS>  accept these signals' findings (comma separated,
                           repeatable; matched like signal labels, by
                           `::`-suffix); an entry suppressing nothing is
                           itself reported
    -h, --help             show this
```
