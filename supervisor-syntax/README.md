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
full: `ready`/`bound` dep markers, `reads:`/`writes:`, `observed`/`beat`/`via`,
`veto`, `discover`, `dataflow:`, `local`, `divisible`, `state:`, `beat_timeout:`,
`beat_window:`, `enabled_if:`, `restart_budget:`, `ready_on_write` always parse.
Whether a build permits a construct is policy, applied by the caller:
`embassy-supervisor-macros` rejects a construct whose feature is off in its `gate`
pass, or defers the rejection to a `compile_error!` when a `#[cfg]` gates the
construct. Every gated construct keeps the `Ident` or literal its rejection points
at, so those errors carry the span the author wrote.

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
  bare `beat` entry, `beat` or `veto` on a `reads:` entry, and beside `discover`
  an entry with no marker (nodes and pools alike).
- Numeric ranges: `slot_timeout:`/`ack_timeout:`/`beat_timeout:`
  at least 1, `beat_window:` in 1..=255, `restart_budget: MAX in WINDOW_MS` with
  `MAX` in 1..=65535 and `WINDOW_MS` at least 1, and a pool's literal
  `min:`/`max:` with `min <= max <= members`. The three timeouts,
  `beat_window:` and both `restart_budget:` numbers carry no suffix
  (`slot_timeout: 5s` is an error).
- A pool lists at least one member mode, and no member carries a `#[cfg]` of
  its own (gate the pool).
- Clause combinations: `task:` vs `spawn:`, clauses that require `task:` (on a
  node `resources:`, `state:`, `exit:`, `cancel`; on a pool `resources:`,
  `state:`, `cancel`), the removed `pool_size:`, `cancel` with mode `Pause`,
  `exit:` on a `pool`, `ready_on_write` without `beat_timeout:` or without an
  `observed beat` write that is gated like it or not at all.
- A repeated clause or marker, `exit:` and `provides:` included, a dep
  repeated in one `deps:` list under the same `#[cfg]`s, a resource name
  repeated in one `resources:` list; list entries separated by commas (a
  trailing comma is fine); an attribute on a clause or a list entry that is not
  `#[cfg(...)]`.
- `default executor` shape: one per graph, never `#[cfg]`-gated (a doc comment
  or a lint attribute passes through), never inside `@fragment` markers (it
  resolves across them at the compose site). Whether a `local` slot's
  declarations share one executor is checked by `embassy-supervisor-macros`,
  not here.
- Fragment bodies: an `observe` default inside `@fragment` markers is refused,
  and [`check_fragment_body`] refuses a `supervisor_fragment!` body that sets
  `name:`, an `observe` default or a `default executor`, all of which are
  graph-wide.

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
