# The two tiers

Every capability above is reachable from one of two positions; mixing them freely,
per task and even per signal, is the normal case.

**The supervisor tier: code that holds its node.** A task declared in the graph
receives its `TaskNode` as the first parameter, and the node is the whole protocol:
lifecycle (`run_cancellable_acked`, `ack_dropped`), health (`beat()`, `set_ready()`,
`report_status()`), and dataflow (`node.put`/`node.get` under `#[dataflow]`). The
more a body routes through its node, the more the graph can check, up to
`discover`, where the body *is* the declaration:

```rust,ignore
#[embassy_supervisor::dataflow]
async fn eskf_task(node: &'static TaskNode) {
    loop {
        let est = fuse(node.get(&crate::signals::IMU_DATA));
        node.put(&crate::signals::ESTIMATE, est);
        node.beat();
    }
}
// node ESKF = Terminate, deps: [IMU ready], task: eskf_task, discover;
```

**The agnostic tier: code that never learns the supervisor exists.** Adopt
supervision without touching the code being supervised:

- a [**`cancel` node**](crate::guide::dsl#cancel-supervisor-unaware-workers) runs a plain
  `async fn` with no node parameter and no handshake; the generated shell owns the
  shutdown race and drops the worker's future in place on stop:

  ```rust,ignore
  async fn telemetry(uart: &mut Uart<'static, Async>) -> ! { loop { /* ... */ } }
  // node TELEM = Terminate, deps: [NET], task: telemetry, cancel,
  //     resources: [UART: Uart<'static, Async>];
  ```

- a node is also a **free-standing health handle**: any code that can see the
  static (an ISR, a callback, a driver) can call `NODE.beat()` or
  `NODE.report_status(..)`, and the same monitor and status endpoint see it like
  any task's;
- [**observed coupling**](crate::guide::dataflow#liveness-and-readiness-by-polling-feature-coupling-observe):
  `writes: [SIG observed beat]` is polled from outside: neither the signal
  crate nor the task body sees the supervisor;
- the **observe facade** ([`embassy-supervisor-observe`](https://crates.io/crates/embassy-supervisor-observe)):
  a signal library implements `Observable`/`Sink`/`Source` in a line each,
  without depending on the supervisor.

Rules of thumb: existing or third-party code goes in as-is through the agnostic
tier (`cancel` + `observed`); code written for this graph takes the node and gets
the exact forms (`beat`, `discover`, `ready_on_write`); a private signal
behind a setter stays private: the setter takes the caller's node and callers
[adopt it](crate::guide::dataflow#the-node-as-the-access-path-feature-dataflow) with
`dataflow: [..]`.
