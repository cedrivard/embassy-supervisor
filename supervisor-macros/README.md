# embassy-supervisor-macros

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor-macros.svg)](https://crates.io/crates/embassy-supervisor-macros)
[![docs.rs](https://docs.rs/embassy-supervisor-macros/badge.svg)](https://docs.rs/embassy-supervisor-macros)

The `supervisor_graph!` proc-macro for
[`embassy-supervisor`](https://crates.io/crates/embassy-supervisor): declare a supervised
task graph once, and get the node `static`s plus a single `GRAPH` bundle whose
**topological order is computed at compile time**. The whole graph is validated at compile
time: dependency cycles, unknown or duplicate dependencies, duplicate node/pool names,
unknown `executor:` slots, malformed spawn forms, and pool bounds (`min <= max <= member
count`, 256-slot cap) are all spanned compile errors — see the
[full list](../supervisor/README.md#limits-and-compile-time-validation).

**Do not depend on this crate directly.** Use `embassy-supervisor` with its default
`macros` feature, which re-exports the macro and forwards the `pool` feature here.
The macro's output references `embassy-supervisor` internals, so the supervisor pins
this crate by exact version — the pair it was tested with.

See the [`embassy-supervisor` documentation](https://docs.rs/embassy-supervisor) for the
macro's surface syntax and examples, and this crate's
[CHANGELOG](CHANGELOG.md) for what each release adds (0.2.0: `executor:` slots,
pool-name deps, trace spawn glue, duplicate-dep/name rejection).

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
