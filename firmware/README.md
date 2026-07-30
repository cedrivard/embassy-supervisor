# RP2350 executor idle-path wake storm: measurement firmware

Embassy's thread executor has never actually slept on RP2350: a long-standing
silicon quirk (reported across raspberrypi/pico-feedback
[#482](https://github.com/raspberrypi/pico-feedback/issues/482), embassy-rs/embassy
[#4818](https://github.com/embassy-rs/embassy/issues/4818) and pico-sdk
[#1812](https://github.com/raspberrypi/pico-sdk/issues/1812)) keeps the idle `WFE`
from ever blocking, so every WFE-idle embassy program on the chip silently burns a
core. This repository holds the firmware and instrumentation used to measure that
behaviour and the effect of the embassy-executor change on
[`cedrivard/embassy:executor-skip-empty-dequeue`](https://github.com/cedrivard/embassy/tree/executor-skip-empty-dequeue),
which makes `RunQueue::dequeue_all` return early instead of calling
`TransferStack::take_all` (a `swap` on the queue head) when the run queue is empty.

Two branches build the **same firmware source** against the two executors; the only
difference between them is the `[patch.crates-io]` table in the root `Cargo.toml`:

| branch | embassy-executor | idle behaviour |
|---|---|---|
| `pr-executor-skip-empty-dequeue-main` | `embassy-rs/embassy` `main` | storms at ~1 MHz |
| `pr-executor-skip-empty-dequeue-fixed` | `cedrivard/embassy` `executor-skip-empty-dequeue` | sleeps |

## The problem

On RP2350, any exclusive access (`ldrex`/`strex`, `ldaex`/`stlex`) posts a monitor
event that acts as an effective `SEV` on its own core, and the event flag is sticky.

The thread executor's run loop is `poll(); wfe`. On `main`, every `poll()` calls
`TransferStack::take_all`, a `swap`, even when the queue is empty, so the pass itself
re-arms the event and the closing `WFE` returns immediately: the executor free-runs at
~1 MHz and never sleeps. Disassembly of this firmware confirms the loop: the `swap`'s
`ldaex`/`stlex` pair sits on every pass; with the fix, the empty pass is a plain load,
a compare, and a return, with no exclusive and no `sev` anywhere between the check and
the `WFE`.

The fix cannot lose a wake: `TransferStack::push_was_empty` reports the
empty -> non-empty transition from the successful CAS's old value, and
`SyncExecutor::enqueue` calls the pender on exactly that transition, so a push racing
the emptiness check always re-pends the executor.

## Measured results (RP2350, 150 MHz, both cores running thread executors)

`passes` below counts scheduler passes; on a thread executor it equals WFE wakeups
exactly. `polls` counts task polls. Identical firmware source, matched conditions
(heartbeat off), only the executor swapped:

| | passes/s | polls/s | polls/pass | busy% | `net` mean poll |
|---|---|---|---|---|---|
| main (storm) | 962167 | 1853 | 0.00 | 14.8 | 59.3 µs |
| fixed | 3385 | 1886 | 0.56 | 14.6 | 57.9 µs |

- **Near idle** (heartbeat off, no network traffic) the thread executor drops from
  ~1.1 M passes/s to **2.5 passes/s**. Idle board draw, no USB and no network,
  heartbeat only: **27.65 mA -> 20.35 mA**.
- **The storm burns only otherwise-idle time.** Busy%, poll rate and `net`'s mean poll
  are unchanged across the A/B (storm cost <= 0.2% busy): the win is power, not
  throughput. Deliberate core-1 interference confirms it from the other side: a
  14 M exclusives/s storm (`bench-excl` feature), or a 57 M SRAM-accesses/s storm
  (`bench-mem`), on core 1 leaves core 0's poll times unchanged.
- **The monitor event is core-local.** The 14 M exclusives/s core-1 storm wakes core 0
  only ~270 times/s, which is core 1's *pass* rate: what crosses cores is the pender's
  explicit `SEV` (architectural, one per empty -> non-empty enqueue), not the erratum
  event. An otherwise-empty core's passes/s therefore mirrors the other core's polls/s.
- **`polls/pass` on the fixed build runs 0.5 to 1.0** and the regime is the erratum at
  one remove: a productive pass's own exclusives (queue swap, waker RMWs) leave the
  sticky event, so the next `WFE` falls through into exactly one empty pass. Sparse
  wakes settle at ~0.5 (0.47-0.54 near idle); saturated back-to-back work absorbs the
  event into a pass that already has a task queued (bench self-wake measures exactly
  1.00); bursty HTTP lands between (0.55-0.63 from ~8 req/s to full `wrk`).
- **Both `dequeue_all` variants validated.** The numbers above are the standard queue;
  with `embassy-executor/scheduler-priority` the same firmware measures 2.2 passes/s
  near idle and a clean load run.

## Reproducing

Hardware: an RP2350 board (this firmware pins the SparkFun IoT RedBoard RP2350,
`rp235xb`; switch `firmware/Cargo.toml` to `rp235xa` for a Pico 2, heartbeat LED is
PIN_25) plus a debug probe and [`probe-rs`](https://probe.rs). Networking is USB
CDC-NCM: device is `10.42.0.61/24`, host setup
`ip addr add 10.42.0.1/24 dev <usb-iface>`.

```sh
cargo build --release -p bootloader
probe-rs download --chip RP235x target/thumbv8m.main-none-eabihf/release/bootloader
cargo run --release -p firmware        # flash + defmt over RTT
```

- **Wake rate over RTT, zero network involvement:** the `watchdog` task prints
  `trace: exec <id> +N passes +M polls in T ms` for every executor every 2 s. On this
  branch expect ~2 M passes per window on both cores; on the fixed branch, single
  digits once traffic stops. `POST /api/heartbeat?ms=0` turns the heartbeat off for a
  true idle floor. Executor ids are addresses: `2007ffe8` core 0 thread, `2000xxxx`
  low = core 1 thread, the remaining static = the `SWI_IRQ_0` interrupt tier.
- **Load run:** `wrk -t1 -c2 -d10s --latency -s firmware/tools/wrk-tasks.lua
  http://10.42.0.61/api/tasks` prints the per-executor passes/polls/busy decomposition
  and per-task poll times.
- **Interference generators:** build with `--features bench-excl` (atomic `fetch_add`
  loop, an exclusive storm) or `--features bench-mem` (atomic load+store loop, the same
  SRAM traffic with no exclusives), then `POST /api/control?node=bench&op=start` /
  `op=stop`. The task runs on core 1 and reports its achieved slice rate on stop, so
  the interference rate is known per run.

## Other embassy targets

The wake storm itself is RP2350-specific; on other chips the measurement shows the
change as a plain optimisation (one fewer read-modify-write per pass, `polls/pass`
near 1) and verifies no regression. The measurement core is portable, the plumbing is
not:

- **Portable as-is:** the `passes`/`polls` counters (embassy-executor trace hooks,
  consumed by `embassy-supervisor`'s `trace-hooks` feature; `Δpasses` == executor loop
  iterations on any thread executor), the watchdog task's 2 s RTT delta-report, the
  bench task and its `bench-excl`/`bench-mem` bodies (plain atomics), and the
  `wrk-tasks.lua` analysis script.
- **Swap the HAL:** replace `embassy-rp` with the target's HAL crate (`embassy-stm32`,
  `embassy-nrf`, ...) in `firmware/Cargo.toml` and adjust `main.rs` peripheral init:
  the heartbeat worker is already generic over `embedded-hal`'s `StatefulOutputPin`,
  so it only needs the board's LED pin.
- **Drop what the target lacks.** Single-core chips: remove the `CORE1` spawn and give
  `bench` `executor:` on the thread executor. No free software interrupt or no
  `InterruptExecutor` port: drop the `HIGH` tier and run `heartbeat` on the thread
  executor. No USB: delete the `net`/`http`/`ota` nodes entirely — the essential
  measurement path (defmt RTT + the watchdog delta-report) has no network dependency,
  and load can come from `bench` alone.
- **Bootloader:** the A/B-partition bootloader is RP2350-specific; on other targets
  build `firmware` as a normal standalone image and feed the watchdog with the
  target's own watchdog driver (or stub the feed out).
