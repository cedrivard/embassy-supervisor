# Elastic pools

`ElasticPool` scales single-instance members between `min` and `max` running instances.
Workers report load (`mark_busy`/`mark_idle` + `request_scale`); the driver wakes on each
scale request or policy deadline (it never polls), asks each pool's `ScalingPolicy` for a
`ScaleAction`, and starts/stops one member accordingly. A member is never grown while one
of its declared dependencies is down (or, with `readiness`, while a `ready`-marked dep is
un-ready).

**The whole driver is one call** when you don't need extra wake sources:
`sup.run(&spawner).await` = a best-effort `start()` + one loop that waits for a scale
request, a control command or (with `bound-deps`) a readiness change and applies each to
completion, so a command never cuts short a pool stop in progress. A bring-up fault, at
boot or on a control `Activate`, is logged and reported as a `Faulted` lifecycle event,
and the node stays down and enabled until the next `Activate`. `run` returns a
`NodeFault` only for a missed shutdown ack or, with `restart-intensity`, a refused
restart, each an app-level escalation, typically `panic!` into a hardware-watchdog reset;
re-entering `run` after a return is safe and retries every enabled node that is down.
Apps that add wake sources call `pool_pass` once per wake
(then `select(wait_scale(), Timer::at(next))` on the instant it returns, `wait_scale()`
alone on `None`) and serve `wait_control` + `apply_control` and, with `bound-deps`,
`wait_bind` + `apply_bind` by hand, one action at a time, and after each bind cascade at
most one queued command before waiting again (a provider that flaps while each cascade
waits would otherwise starve control), with `monitor()` beside them under
`liveness-monitor` (see the [Quickstart](crate#quickstart)). A bare
`select(sup.run_pools(&spawner), wait_control())` drops `run_pools` mid-action when a
command arrives, so a member being stopped at that moment gets no `Stopped` event and no
ack-timeout check; `pool_pass` returns between actions, which is why a hand-written loop
uses it.

The built-in `DeferredShrink` policy grows immediately when saturated (no idle member, below
`max`) and shrinks only after an idle surplus has persisted for a configurable cooldown:
responsive up, lazy down. One idle spare is the stable dead-band, so a single spare never
flaps. Swap in your own policy by implementing `ScalingPolicy` (a sync, allocation-free
decision fn).
