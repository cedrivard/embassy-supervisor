# embassy-supervisor

[![crates.io](https://img.shields.io/crates/v/embassy-supervisor.svg)](https://crates.io/crates/embassy-supervisor)
[![docs.rs](https://docs.rs/embassy-supervisor/badge.svg)](https://docs.rs/embassy-supervisor)

A generic, **HAL-agnostic** task-lifecycle supervisor for the [embassy](https://embassy.dev)
async embedded framework. `no_std`, no allocator, no board crates — it compiles for any embassy
target. The only third-party deps are pure-embassy crates (`embassy-executor`/`-sync`/`-time`/
`-futures`) and `portable-atomic`.

## What it does

- **Dependency-ordered lifecycle** — declare the graph with the `supervisor_graph!` macro; it
  computes the topological order **at compile time** (a dependency cycle is a *compile error*), and
  the supervisor brings tasks up in dependency order and tears dependents down before the things
  they depend on.
- **Lifecycle modes** — `Terminate` (started at boot, restartable), `Pause` (park/resume while
  keeping a held resource), `OnDemand` (started on demand to scale a pool).
- **Elastic pools** *(feature `pool`)* — `ElasticPool` scales a set of single-instance worker
  nodes with load via a swappable `ScalingPolicy` (e.g. `DeferredShrink`), within a fixed budget.
- **Runtime control** *(feature `control`)* — drive start/stop/pause/resume from anywhere (an HTTP
  endpoint, a button, …) through a decoupled mailbox (`request_control` / `apply_control`) that
  honors dependencies and pool membership.

The supervisor deliberately does **not** allocate, own a HAL, manage power states, or know what your
tasks do — it orchestrates their *lifecycle* and leaves the rest to you.

## Quickstart

```rust,ignore
use embassy_executor::Spawner;
use embassy_supervisor::{supervisor_graph, Supervisor, wait_control};

// Declare the graph once: `supervisor_graph!` generates the node `static`s and the
// compile-time `ALL_NODES` / `DEPS` / `ORDER`. Each `spawn:` names a task fn that is
// `s.spawn`ed with the node; `app` depends on `net`.
supervisor_graph! {
    node NET = Terminate, deps: [], spawn: net_task;
    node APP = Terminate, deps: [NET], spawn: app_task;
}

#[embassy_executor::task]
async fn supervisor_task(spawner: Spawner) {
    // Infallible: the order is precomputed, so a dependency cycle is a compile error.
    let sup = Supervisor::new(&ALL_NODES, &DEPS, ORDER);
    sup.start(spawner).expect("initial spawn");   // brings up `net`, then `app`
    loop {
        let cmd = wait_control().await;           // runtime control requests
        sup.apply_control(cmd, spawner).await;    // applied in dependency order
    }
}
```

## Cargo features

| feature   | default | what it adds |
|-----------|:-------:|--------------|
| `control` |    ✓    | runtime control plane (`ControlOp`, `request_control`, `apply_control`) |
| `pool`    |    ✓    | elastic worker pools (`ElasticPool`, `with_pools`, `run_pools`) |
| `macros`  |    ✓    | the `supervisor_graph!` graph-declaration macro |
| `defmt`   |         | route the supervisor's logs through `defmt` (otherwise the log macros are no-ops) |

`default-features = false` gives a minimal core that only does dependency-ordered
bring-up/teardown — dropping the control plane and pools trims flash and a couple of statics.

## `no_std` / MSRV

`#![no_std]` and `#![forbid(unsafe_code)]`. Requires Rust 1.85+ (edition 2024). The embassy
dependencies are pre-1.0 (`embassy-executor` 0.10, `embassy-sync` 0.8, `embassy-time` 0.5), so a
consuming application must use compatible embassy minor versions.

## Full example

The [`firmware`](https://github.com/cedrivard/embassy-supervisor/tree/main/firmware) crate in the
repository is a complete working application on an RP2350 — USB-CDC-NCM networking, an HTTP control
plane, an elastic worker pool, and OTA firmware update — all driven by this supervisor.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
