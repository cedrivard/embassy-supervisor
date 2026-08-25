# embassy-supervisor-macros

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor-macros.svg)](https://crates.io/crates/embassy-supervisor-macros)
[![docs.rs](https://docs.rs/embassy-supervisor-macros/badge.svg)](https://docs.rs/embassy-supervisor-macros)

Proc-macro crate for [`embassy-supervisor`]: the `supervisor_graph!` graph declaration macro plus `compose_graph!` for fragment assembly. Do not depend on this crate directly — use `embassy_supervisor` with its default `macros` feature, which re-exports the macro and forwards features here.

**The macro's output references `embassy-supervisor` internals, so the supervisor pins this crate by exact version.** Use it only as a dependency of `embassy-supervisor`.

## Documentation

What the macro emits and the runtime contract behind it:
[The DSL](https://embassy-supervisor.github.io/concepts/dsl/) for the input
syntax, [Task lifecycle](https://embassy-supervisor.github.io/concepts/lifecycle/)
for the generated glue's semantics.