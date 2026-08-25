# embassy-supervisor-syntax

The parser and AST for the `supervisor_graph!` DSL of
[`embassy-supervisor`](https://docs.rs/embassy-supervisor): turn graph
declarations into [`GraphSpec`] values and reject malformed ones with
span-attached errors.

## Not a stable API

The AST is an internal contract with `embassy-supervisor-macros`, which pins
this crate by **exact version**. Fields and variants change whenever the graph
syntax does. Depend on it directly only if you are willing to track it.

## What parsing does not decide

**Feature gating.** This crate carries no features and accepts the grammar in
full: `ready`/`bound` dep markers, `observed`/`beat`/`via`, `discover`,
`dataflow:`, `local`, `state:`, `beat_timeout:`, `ready_on_write` always parse.
Whether a build permits a construct is policy, applied by the caller:
`embassy-supervisor-macros` rejects gated constructs in its `gate` pass. Every
gated construct keeps the `Ident` or literal its rejection points at, so those
errors carry the span the author wrote.

**Name resolution.** Unknown deps, duplicate names, executor slot references,
and the 256-node-slot cap (pool members included) are properties of a whole
graph, checked by `embassy-supervisor-macros` while expanding. A
`supervisor_fragment!` legitimately names nodes it does not contain; they
resolve at the compose site, so parsing it in isolation must succeed.

## What parsing does check

Shape, which is part of the grammar rather than any build's policy. All errors
are span-attached:

- Empty clause lists (`reads:`, `writes:`, `resources:`, `dataflow:`,
  `provides:` must each declare at least one entry; omit the clause instead).
- A repeated signal path, or one path declared both bare and indexed (`&ARR`
  and `&ARR[0]` share an address, so nothing downstream could tell them apart).
- Marker shape: `bound` without `ready`, `beat`/`via` without `observed`, a
  bare `beat` entry, `beat` on a `reads:` entry.
- Numeric ranges: `slot_timeout:`/`ack_timeout:`/`beat_timeout:`/`pool_size:`
  at least 1, `beat_window:` in 1..=255.
- Clause combinations: `task:` vs `spawn:`, clauses that require `task:`
  (`pool_size:`, `resources:`, `state:`, `exit:`, `cancel`), `cancel` with
  `Mode::Pause`, `exit:` on a `pool`, `local` with `executor:`,
  `ready_on_write` without `beat_timeout:` and an `observed beat` write.

## Usage

```rust
use embassy_supervisor_syntax::{GraphSpec, Item};

let spec: GraphSpec = syn::parse_str(
    "node NET = Terminate, task: net_task;\n\
     node HTTP = Terminate, deps: [NET ready], task: http_task;",
)?;

for item in &spec.items {
    if let Item::Node(n) = item {
        println!("{} depends on {} node(s)", n.ident, n.deps.len());
    }
}
# Ok::<(), syn::Error>(())
```

## Fragment helpers

`supervisor_fragment!` items reach a compose site as tokens. Two helpers move
them between spellings:

- [`normalize_fragment_crate`] rewrites every bare `crate` to `$crate`, so a
  fragment forwarded into another crate still names its own items.
- [`substitute_dollar_crate`] replaces every `$crate` with a caller-chosen
  token stream, so fragment items parse as a [`GraphSpec`] (the macro
  substitutes a placeholder; a source reader substitutes the compose crate).
