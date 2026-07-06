# embassy-supervisor demo firmware (RP2350)

A complete embedded application demonstrating the
[`embassy-supervisor`](https://crates.io/crates/embassy-supervisor) crate driving a
real [embassy](https://embassy.dev) firmware. The supervisor is the star; this
firmware keeps each task thin (wrapping third-party crates) so the orchestration
stays in view.

## Table of Contents

1. [Overview — what this demo shows](#overview--what-this-demo-shows)
2. [The supervised task graph](#the-supervised-task-graph)
   - [Executors](#executors)
   - [Node-by-node breakdown](#node-by-node-breakdown)
   - [Supervisor features exercised](#supervisor-features-exercised)
3. [Build & run (RP2350)](#build--run-rp2350)
4. [Host network setup (USB-net)](#host-network-setup-usb-net)
5. [Exercise the supervisor](#exercise-the-supervisor)
6. [Stress-testing the HTTP service](#stress-testing-the-http-service)
7. [Interpreting the observability data](#interpreting-the-observability-data)
8. [OTA update](#ota-update)
9. [Portability](#portability)
10. [Implementation notes](#implementation-notes)

## Overview — what this demo shows

`embassy-supervisor` provides, at runtime:

- **Dependency-ordered bring-up / teardown** (topological sort of a task graph,
  computed at compile time by `supervisor_graph!`).
- **Lifecycle modes** — `Terminate` (exit + respawn), `Pause` (park + resume,
  keeping a held resource), `OnDemand` (pool workers brought up/down with load) —
  see the [lifecycle reference](../supervisor/README.md#lifecycle-reference).
- **Elastic task pools** — grow immediately under load, shrink after a cooldown.
- **Multi-executor, multi-core placement** — `executor:` slots route nodes onto
  an interrupt-priority tier or the second core, all driven by one supervisor.
- **Dependency- and pool-honoring control** — start/stop/pause/resume a task (or
  a whole pool) at runtime; a stop cascades through dependents, a start through
  dependencies, and a manual stop "sticks" against autoscaling.
- **Trace observability** — per-node CPU% / poll counters and per-executor
  busy/idle/pass accounting, surfaced over HTTP.

This firmware exercises all of the above on an RP2350: USB networking, an HTTP
control/observability plane, an elastic pool of keep-alive HTTP workers, a
Pause-mode heartbeat LED, a detached watchdog daemon, a second-core benchmark
load, and an A/B OTA update path with a run-last confirm node. It depends on
`embassy-supervisor` via a path dependency with the `defmt`, `trace-hooks`, and
`trace-nested` features enabled (`control`/`pool`/`macros` come from the crate's
defaults — see the [features table](../supervisor/README.md#cargo-features)).

Repository layout:

- [`embassy-supervisor`](../supervisor) — the generalized, HAL-agnostic supervisor
  crate this firmware demonstrates. Zero board or application specifics; it
  compiles for any embassy target.
- `firmware/` (this crate) — the RP2350 demo application; the only RP-specific code.
- `bootloader/` — the embassy-boot-rp A/B bootloader (the ROM boots it; it swaps
  DFU↔ACTIVE on a pending update).

## The supervised task graph

The graph is declared once, in `firmware/src/main.rs`, by `supervisor_graph!` —
the single source of nodes, deps, executors, pool, and order (a dependency cycle
is a *compile* error):

```
watchdog  (Terminate, detached daemon)              heartbeat (Pause, executor HIGH)
bench     (Terminate, executor CORE1, disabled)

net (Terminate) ──┬── http pool (1 Terminate floor + 1 OnDemand) ── ota-confirm (Terminate,
                  │                                                  detached, runs last)
                  └── ota (Terminate, disabled at boot; control-started)
```

The `http` pool is both the worker pool and the control/observability plane: each
worker is an HTTP/1.1 keep-alive server, and the pool grows under concurrent load.

### Executors

Three executors, all fed by the same core-0 supervisor — a node's placement is a
one-line `executor:` field in the graph:

| Graph slot | Executor | Runs | Nodes |
|---|---|---|---|
| *(default)* | core-0 thread executor (`#[embassy_executor::main]`) | thread mode, core 0 | `watchdog`, `net`, `http0..1`, `ota`, `ota-confirm`, the supervisor task |
| `HIGH` | `InterruptExecutor` on `SWI_IRQ_0` at priority P2 | preempts the thread executor | `heartbeat` |
| `CORE1` | thread executor on core 1 (`spawn_core1`; publishes a `SendSpawner` into the slot) | core 1 | `bench` |

### Node-by-node breakdown

| Node | Mode | Deps | Executor | Boot state | How the supervisor manages it |
|---|---|---|---|---|---|
| `watchdog` | Terminate | — | thread | started | Detaches itself as its first act (`set_detached(true)`): the supervisor starts it once and then never tears it down, respawns it, or includes it in cascades — a self-managed daemon. |
| `net` | Terminate | — | thread | started | Root of the data plane: torn down last among its subtree, dependents (`http`, `ota`) cascaded down first. Stopping it returns its whole ~16 KB heap budget. |
| `heartbeat` | Pause | — | `HIGH` | started | Pause parks the task (it acks and keeps its LED `Output`); resume continues the same future. Never respawned. |
| `http0` | Terminate | `net` | thread | started | The pool floor: always on while `net` is up; a manual stop of the floor seeds a whole-pool deactivate. |
| `http1` | OnDemand | `net` | thread | stopped | Elastic burst worker: `ElasticPool<DeferredShrink>` (4 s cooldown, `min: 1, max: 2`) grows it when every running worker is busy, shrinks it back after the cooldown. |
| `ota` | Terminate | `net` | thread | **disabled** | Control-started (`Activate` via `POST /api/ota` or the dashboard). Detaches itself at start, so once running it is uninterruptible; it then drains `http` and `net` itself and exits only via reset. |
| `bench` | Terminate | — | `CORE1` | **disabled** | Control-started compute load on core 1: the core-0 supervisor spawns/stops it cross-core through the `CORE1` spawner slot. Honors shutdown (`ack_dropped`) like any Terminate node. |
| `ota-confirm` | Terminate | `http` (= pool floor) | thread | started | Depending on the pool name resolves to its floor member, so this node is spawned **last** in topological order. It detaches, waits for the network to come up, calls `mark_booted` to confirm the running image, and exits — a run-once job. |

What each node demonstrates:

- **`watchdog`** (`watchdog.rs`) — the *detached daemon* pattern. It feeds the
  bootloader's 8 s rollback watchdog every 2 s, so it must survive every
  teardown/respawn cycle — exactly what `set_detached` guarantees. It doubles as
  a post-hoc stall detector, warning when any node's `max_poll_ticks` watermark
  crosses 100 ms.
- **`net`** (`net.rs`) — a *reclaimable subsystem*, not a fixed reservation. The
  task owns the entire USB-CDC-NCM + embassy-net bring-up: on start it
  heap-allocates all buffers (descriptors, the ~12 KB packet pool, socket
  storage); on stop it drops them, returning the whole budget.
- **`heartbeat`** (`heartbeat.rs`) — *Pause/Resume with a retained resource*: the
  LED `Output` is owned across pause→resume, never re-acquired. Its blink period
  is a runtime parameter (`POST /api/heartbeat?ms=`). It is also the firmware's
  **live consumer of `embassy_supervisor::trace`**: on every blink tick it walks
  `trace::executors()` and calls `trace::stalled_task(id, 100ms)` — and because it
  runs on the `HIGH` interrupt tier it still gets CPU while the thread executor is
  wedged, so it names the blocking task *while the stall is happening*, before the
  watchdog resets (the watchdog's `max_poll_ticks` watermark is the post-hoc
  complement).
- **`http` pool** (`http.rs`) — *elastic pool + socket & heap budgeting*. Each
  worker owns one embassy-net socket and heap I/O buffers, so the pool scales
  within the fixed `StackResources` budget and its heap footprint tracks the live
  worker count. `mark_busy`/`mark_idle` drive the scaling.
- **`ota`** (`ota.rs`) — the supervisor's signature *drain-to-free-budget* move:
  a disabled-at-boot node that, once control-started, orchestrates its own
  resource draining (see [OTA update](#ota-update)).
- **`bench`** (`bench.rs`) — *multi-core placement*: yield-chunked xorshift
  busywork that pins core 1's executor near 100% in-poll while core 0's numbers
  are untouched. Each task lives on one core; nothing migrates.
- **`ota-confirm`** (`main.rs`) — *deps-on-pool ordering + run-once*: by
  depending on `http` it starts after the whole graph is up; an update broken
  enough not to reach it never confirms, and the bootloader rolls back.

### Supervisor features exercised

| Supervisor feature | Where |
|---|---|
| Task dependencies | `http`/`ota` depend on `net`; ordered start/teardown |
| Dynamic task pools | `http` `ElasticPool<DeferredShrink>` (`http.rs`) |
| Deps on a pool name | `ota-confirm` depends on `http` → its floor member, so it runs last |
| Detached daemon | `watchdog` — started once, excluded from every cascade |
| Detached teardown | `ota` outlives the `net` it tears down (`set_detached`) |
| Lifecycle: Pause/Resume | `heartbeat` keeps its LED `Output` across a pause |
| Lifecycle: control-started | `ota` and `bench` are pre-disabled, started by control |
| Multi-executor tier | `heartbeat` on the `HIGH` `InterruptExecutor` (SWI_IRQ_0 @ P2) |
| Multi-core placement | `bench` on `CORE1` via the graph's spawner slot |
| Resource: sockets | the pool scales within the fixed `StackResources` budget |
| Resource: heap (budgeted) | `ota` drains `http` + `net` to free the arena for the decode |
| Runtime control | `POST /api/control` → `embassy_supervisor::request_control` |
| Trace observability | `GET /api/tasks` (and the dashboard at `/`) — CPU%, max-poll, executor stats |

## Build & run (RP2350)

Requires a debug probe + [`probe-rs`](https://probe.rs) (defmt logs stream over
the probe; the USB port is the network link).

The firmware always runs from the **ACTIVE partition under the bootloader** (it is
linked at `0x10021000` and cannot boot standalone), so a first flash installs the
**bootloader + firmware pair**. The bootloader arms an 8 s watchdog as its OTA
rollback safety; the firmware's `watchdog` node feeds it, so a healthy image stays
up and a hung one resets and rolls back.

```sh
# Board variant: firmware/Cargo.toml uses embassy-rp `rp235xb` (SparkFun IoT
# RedBoard RP2350 / 48-GPIO). For a standard Raspberry Pi Pico 2, switch it to
# `rp235xa`. The heartbeat LED is PIN_25 (adjust in heartbeat.rs for your board).

# 1. Build both crates (release: matches the OTA heap budget and image size)
cargo build --release -p bootloader
cargo build --release -p firmware

# 2. Wipe flash so STATE/DFU start blank (no stale swap pending), then flash the
#    bootloader once (-> 0x10000000)
probe-rs erase --chip RP235x
probe-rs download --chip RP235x target/thumbv8m.main-none-eabihf/release/bootloader

# 3. Flash + run the firmware (-> ACTIVE); resets through ROM -> bootloader -> ACTIVE
#    and streams defmt with the firmware's symbols
cargo run --release -p firmware
```

The bootloader rarely changes, so iterating on the firmware afterward is just
`cargo run --release -p firmware` again — it re-flashes ACTIVE and reboots through
the existing bootloader. `probe-rs erase --chip RP235x` + steps 2–3 is also the
recovery path if a bad image bricks the boot. To only build (no probe):

```sh
cargo build --workspace          # thumbv8m.main-none-eabihf
cargo build -p embassy-supervisor --target x86_64-unknown-linux-gnu   # crate is host-buildable
```

## Host network setup (USB-net)

Networking is **USB-CDC-NCM** (TCP/IP over the USB cable) — a deliberate demo
choice for ease of reuse and testing: no extra hardware or wireless setup, and it
works on any embassy MCU with USB. A real application would swap in a wireless
chip; only the `net` task changes — the rest of the graph is unaffected.

The device uses a static IP `10.42.0.61/24`. Point the host's USB ethernet
interface at the same subnet, then browse to the device:

```sh
ip addr add 10.42.0.1/24 dev usb0      # interface name varies (enxXX… on some hosts)
ip link set usb0 up
xdg-open http://10.42.0.61/             # task view + stop/start buttons
```

## Exercise the supervisor

- **Dynamic pool:** hold several concurrent connections open to the HTTP port so
  every worker is busy, and watch the pool grow `1 → 2` in the task view (free
  heap drops as workers spawn), then shrink ~4 s after they close (heap returns):
  ```sh
  for i in $(seq 6); do (sleep 12) | nc 10.42.0.61 80 & done   # 6 clients -> pool caps at 4
  ```
- **Dependency cascade:** stop `net` (button, or
  `curl -XPOST 'http://10.42.0.61/api/control?node=net&op=stop'`) and watch the
  whole `http` pool torn down first, then `net` itself — free heap jumps as net
  returns its ~16 KB budget. (Since `net` hosts the control plane, this also drops
  the dashboard; it illustrates the root drain, not a recoverable-over-HTTP stop.)
- **Pause/Resume:** pause `heartbeat` (LED stops; the GPIO handle is retained),
  then resume it.
- **Multi-core load:** start `bench`
  (`curl -XPOST 'http://10.42.0.61/api/control?node=bench&op=start'`) and watch
  core 1's executor line jump from idle to ~100% busy (in-poll) while core 0's
  numbers are untouched; stop it and core 1 goes quiet again.
- **Runtime parameter:** change the heartbeat without a rebuild —
  `?ms=` is `>0` blink half-period, `0` LED off, `<0` LED on, applied immediately:
  ```sh
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=100'   # fast blink
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=0'     # off
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=-1'    # on
  ```

## Stress-testing the HTTP service

The `/api/tasks` endpoint doubles as a load target and a self-report: every
response carries the full trace snapshot (heap, per-executor and per-task
counters), so hammering it with [`wrk`](https://github.com/wg/wrk) both exercises
the elastic worker pool *and* streams back the numbers to judge how it held up.
The companion Lua script (`firmware/tools/wrk-tasks.lua`) parses those snapshots
and prints an analysis when the run ends.

### Prerequisites

Install `wrk` (a scriptable HTTP benchmarking tool with a Lua/LuaJIT engine):

```sh
sudo apt install wrk      # Debian/Ubuntu
sudo dnf install wrk      # Fedora
brew install wrk          # macOS
sudo pacman -S wrk        # Arch (or the AUR)
```

### The command

From the repo root, pointing at the device:

```sh
wrk -t1 -c2 -d10s --latency -s firmware/tools/wrk-tasks.lua http://10.42.0.61/api/tasks
```

Why these flags:

- **`-c2` matches `POOL_MAX`.** The HTTP pool tops out at two workers
  (`POOL_MAX = 2` in `firmware/src/http.rs`), one embassy-net socket per worker,
  and that ceiling is also the socket budget. Two keep-alive connections give
  every worker exactly one connection to serve, driving the `ElasticPool` to its
  fully-grown steady state without piling extra connections onto the `accept`
  backlog. Go higher and you stop measuring per-worker behaviour and start
  measuring accept contention / connection churn instead.
- **`-t1` gives the cleanest report.** Each wrk thread runs an isolated Lua
  state, and `done()` runs in yet another; nested tables can't be copied across
  states reliably. With a single thread there is exactly one rendered report to
  print, and one thread easily saturates two keep-alive connections against an
  embedded target.
- **`--latency`** adds wrk's own latency percentile table to the summary,
  alongside the script's device-side analysis.
- **`-d10s` is plenty:** the device counters are wrapping `u32` ticks that only
  wrap after ~71 min of uptime at 1 MHz, so any short run is wrap-safe. The only
  thing that breaks the first-vs-last comparison is a run that straddles a device
  reboot.

### What the script measures

**Per response** (validated on every sample):

- **Well-formedness / truncation.** `heap_free`, `tick_hz`, and `now_ticks` must
  be present and the body must end in `]}`. Anything else means the device
  truncated the JSON — the body outgrew the worker's TX buffer — and increments
  `malformed/truncated`.
- **Heap headroom.** Tracks `heap_free` min/max across the run; the *min* is the
  free heap at peak pool load.
- **Counter monotonicity.** `now_ticks` must advance and each task's `exec_ticks`
  must only accumulate. A wrap-safe backward step is counted as a **regression**
  — a trace-attribution bug, not normal wrap.

**Over the whole run** (first-vs-last wrap-safe diffs):

- **Per task:** CPU% (`exec_ticks` delta / window), poll count, average poll
  duration and **max poll duration** in µs (via `tick_hz`).
- **Per executor:** `busy% = in-poll% + overhead%` (in-poll is time inside task
  polls; overhead is scheduler bookkeeping, trace hooks, and ISRs landing between
  polls), plus polls/s, passes/s, and polls/pass.
- **Unsupervised share.** With exactly one executor the script attributes
  `in-poll minus the sum of supervised tasks` to unsupervised work (tasks the
  graph doesn't track); with multiple executors this can't be pinned to one, so
  it's omitted.

### Reading a sample report

```
==== /api/tasks analysis (wrk thread 1) ====
samples 424 | non-200 0 | malformed/truncated 0 | counter regressions 0
body bytes  min 1655 / max 1656 (worker tx buffer must hold max + headers)
heap_free   min 5284 / max 5284 B (min = headroom at peak load)
window      9.9 s of device time (tick_hz 1000000)
executor 200093b0: 0.0% busy = 0.0% in-poll + 0.0% overhead (scheduler + hooks + inter-poll ISRs)
executor 200093b0: 2 polls/s, 2 passes/s, 1.00 polls/pass
executor 2007ffe8: 23.6% busy = 21.6% in-poll + 2.0% overhead (scheduler + hooks + inter-poll ISRs)
executor 2007ffe8: 1832 polls/s, 850993 passes/s, 0.00 polls/pass
executor 20000360: 0.0% busy = 0.0% in-poll + 0.0% overhead (scheduler + hooks + inter-poll ISRs)
executor 20000360: 0 polls/s, 869433 passes/s, 0.00 polls/pass
task            cpu%      polls  avg poll us  max poll us
bench          0.00%          0          0.0          0.0
heartbeat      0.01%         20         38.1         68.0
http0          1.51%        213        705.8        748.0
http1          1.53%        212        715.8        795.0
net           18.57%      17783        103.8        696.0
ota            0.00%          0          0.0          0.0
ota-confirm    0.00%          0          0.0         21.0
watchdog       0.00%          5         36.2         62.0
```

**Good looks like:** `non-200 0`, `malformed/truncated 0`, `counter regressions 0`;
`body max` comfortably below the worker's 2560-byte TX buffer minus header bytes;
`heap_free min` steady and well clear of zero; task CPU% roughly balanced across
`http0..http1`; max poll µs bounded.

**Bad looks like:**

- **`malformed/truncated > 0`** — the JSON body no longer fits the worker's TX
  buffer (1440 B, `tx` in `http_task`). Shrink the body or grow the buffer.
- **`heap_free min` approaching 0** — the pool grew but the heap can't sustain a
  fully-grown pool under load; an allocation is about to fail. This is the exact
  failure mode the TX/body sizing was chosen to avoid.
- **`counter regressions > 0`** — a counter stepped backward beyond wrap
  tolerance: a trace attribution bug, not load-related — worth investigating
  regardless of throughput.
- **`non-200 > 0`** — connections dropped or errored under load.
- **`busy%` pinned near 100% with high `overhead%`** — the executor is saturated
  and spending a large share outside polls; a runaway max-poll on one task points
  at a specific culprit.

## Interpreting the observability data

The firmware enables `embassy-supervisor`'s `trace-hooks` + `trace-nested`
(see the [supervisor features](../supervisor/README.md#cargo-features)), so
`GET /api/tasks` — and the dashboard rendering it — reports per-node and
per-executor counters. All counters are **wrapping u32 ticks**: to get a rate,
sample twice and `wrapping_sub` (the dashboard and the wrk script both do this),
or divide a cumulative counter by uptime when nothing has wrapped. `tick_hz`
converts ticks to time (here 1 MHz → 1 tick = 1 µs).

### System / heap

- `heap_total` / `heap_free` — arena size (32768) and bytes currently free.
  **Healthy:** free oscillates around a steady baseline across load.
  **Concerning:** a monotonic downward trend between idle points (a leak), or
  dips near 0 under peak load (allocation-failure risk).
- `tick_hz` / `now_ticks` — tick unit and device uptime; use `now_ticks` as the
  denominator for whole-uptime rates.

### Per-executor

Each executor reports `id, idle_ticks, exec_ticks, polls, passes`. Over a window
of `dt` ticks:

```
busy         = dt - Δidle_ticks            (executor not sleeping)
in-poll      = Δexec_ticks                 (inside task polls, supervised or not)
overhead     = busy - Δexec_ticks          (bookkeeping + trace hooks + ISRs between polls)
unsupervised = Δexec_ticks - Σ Δnode.exec_ticks   (polls that map to no node)
busy%        = in-poll% + overhead%
```

- `passes` counts scheduler passes; `polls` counts completed task polls, so
  **`polls / passes` is the mean useful polls per pass**. Empty passes (woken but
  nothing runnable) are booked as **idle**, not overhead.
- **The single most diagnostic comparison is `passes/s` vs `polls/s`:** near-equal
  (ratio ≈ 1) is healthy — the executor wakes only for work. `passes/s` far above
  `polls/s` is a **wake storm** (see [below](#note-on-rp2350)). Tick-based
  idle% alone hides a storm completely, because empty passes count as idle.
- **Overhead as a share of busy** grows with poll rate (~13% of a 150 MHz core
  measured at ~8k polls/s under HTTP load); it ballooning means the hooks and
  bookkeeping are eating the core — expected only at very high poll rates.
- **Unsupervised share** should be near zero (nearly all poll time maps to named
  nodes); a large share means significant work in tasks outside the graph.

Caveats: the accounting is preemption-naive by default — a preempted
thread-executor poll absorbs the preemptor's CPU, idle is per-executor not
per-core, and hardware-ISR time is invisible (it inflates whichever node was
mid-poll, else lands in overhead). This firmware enables `trace-nested`, which
makes nested/preempted attribution preemption-exact.

### Per-task

Each task reports `name, mode, running, busy, disabled, detached, exec_ticks,
polls, max_poll_ticks, deps`.

- **CPU% = Δ`exec_ticks` / Δ`now_ticks`; mean poll = `exec_ticks` / `polls`.**
- **`max_poll_ticks` is the key health signal** — the longest single poll ever
  seen, the "never yields" watermark. A poll should be microseconds; a value in
  the many-ms range means the task ran without hitting an `.await` and starved
  its executor for that long. It is the post-hoc twin of `stalled_task()`, which
  this firmware uses live in two places: `heartbeat` (on the `HIGH` tier, so it
  can name a task wedging the thread executor *while it happens*) and `watchdog`
  (post-hoc watermark warnings every feed cycle).
- **Healthy:** `max_poll_ticks` in the hundreds of µs; CPU% small and
  proportional to the task's job; on-demand pool members sitting idle at 0 until
  pulled in. **Concerning:** `max_poll_ticks` ≫ ms (busy-looping without
  yielding); one task's CPU% approaching its executor's whole busy% (it dominates
  the core); `running=true` but `polls` frozen across samples (wedged task).

Quick recipe: (1) convert with `tick_hz`; (2) per executor, compare `passes/s`
to `polls/s` — a big gap is a wake storm; (3) split `busy%` into in-poll and
overhead, and check the unsupervised share; (4) per task, compute CPU% and mean
poll, and treat a large `max_poll_ticks` as the flag for a task that doesn't
yield.

### Note on RP2350

⚠️ **On RP2350, the executor "idle %" is NOT sleep.** RP2350 has a silicon quirk where any
exclusive-access atomic (`ldaex`/`strex`) raises a global-monitor event — an effective
`SEV` — so every atomic in the executor's idle loop (the critical-section spinlock, task
flags) makes the following `WFE` return immediately. The thread executor therefore
free-runs at ~1 MHz (`polls/pass` in the wrk report is far below 1) and never actually
sleeps, regardless of how little work there is. Read `idle %` as "WFE-spin", not power
saving. This is not specific to this firmware or the supervisor — it affects any
WFE-idle embassy/pico-SDK program on RP2350. For genuine low power use the `powman`
peripheral (deep sleep), not WFE.


- raspberrypi/pico-feedback [#482](https://github.com/raspberrypi/pico-feedback/issues/482)
- embassy-rs/embassy [#4818](https://github.com/embassy-rs/embassy/issues/4818)
- pico-sdk [#1812](https://github.com/raspberrypi/pico-sdk/issues/1812)

## OTA update

A/B firmware update over USB-net with `embassy-boot-rp` rollback. `ota` is a
`Terminate` node pre-disabled at boot, so it sits stopped until control starts it.
Once started, **the node orchestrates its own resource draining** — the
supervisor's signature move:

1. `POST /api/ota[?ip=&port=&path=]` records a download target (each part defaults:
   gateway `10.42.0.1`, port `8000`, path `/fw.zst`) and issues `Activate(ota)`.
   Starting the node straight from the dashboard (or `/api/control?node=ota&op=start`)
   works too — with no target set, the task falls back to the same defaults.
2. The `ota` task **detaches itself as its first act** — from here on it is
   uninterruptible (no control op can stop it mid-update; its only exits are the
   reset in step 4). It then **drains the http pool** (waiting via `is_running`
   for the workers' sockets to free), pulls the **zstd** image with `reqwless`
   over a socket it opens by IP, and streams it into a 128 KB **scratch** flash
   region.
3. It then **drains `net`** — it decodes from flash, not the network, so net's
   ~16 KB is reclaimed for the decoder. Being detached, net's teardown doesn't
   cascade back into the still-running `ota` task.
4. `ruzstd` (`windowLog=11`) decodes the scratch image into the DFU partition with
   nearly the whole arena free, arms the swap (`mark_updated`), and resets. On
   failure it resets *without* arming the swap — a clean recovery into the
   current image.
5. The bootloader swaps DFU→ACTIVE; on the next boot the `watchdog` node feeds
   the rollback watchdog and, once the network is up, the run-last `ota-confirm`
   node calls `mark_booted` to confirm — otherwise the bootloader rolls back.

Build the update image (a flat binary of the ACTIVE-located firmware, zstd'd):

```sh
cargo build --release -p firmware
rust-objcopy -O binary target/thumbv8m.main-none-eabihf/release/firmware /tmp/fw.bin
# cap the window at 11 and drop the checksum (ruzstd is built without the hash feature)
zstd -19 --no-check --zstd=wlog=11 /tmp/fw.bin -o /tmp/fw.zst
ls -l /tmp/fw.zst        # must be < 128 KB (the scratch region)
```

Serve it from the host and trigger the update (make a visible change first — e.g.
the dashboard `<h1>` — so you can tell the new image apart):

```sh
cd /tmp && python3 -m http.server 8000     # serves /tmp/fw.zst at the gateway default
curl -XPOST 'http://10.42.0.61/api/ota'    # no params -> gateway:8000/fw.zst
# or click the `ota` row's start button on the dashboard; or override:
#   curl -XPOST 'http://10.42.0.61/api/ota?ip=10.42.0.9&port=9000&path=/v2.zst'
```

The device acks `downloading`, drops off (decode + reset), and comes back running
the new image. **Rollback:** an image that crashes or hangs before `mark_booted`
stops feeding the watchdog → reset → the bootloader reverts to the previous image.
`probe-rs erase --chip RP235x` + a fresh flash is the recovery path if needed.

## Portability

The `embassy-supervisor` crate is HAL-agnostic and reused verbatim on any embassy
target. Porting this firmware to another MCU means swapping `embassy-rp` for the
target HAL, the USB init, and `embassy-boot-<mcu>` — the supervisor, USB-net,
HTTP plane, OTA flow, and the whole task graph stay.

## Implementation notes

- `embassy-supervisor`'s logging is an optional `defmt` feature (no-op otherwise),
  which this firmware enables so the supervisor's lifecycle events show up over RTT.
- The HTTP plane is a small hand-rolled HTTP/1.1 handler on stable Rust, with
  keep-alive (one connection serves many requests; reaped on `Connection: close`
  or a 10 s idle timeout). `picoserve` was considered but its ergonomic embassy
  router needs the nightly `impl_trait_in_assoc_type` feature.
- Each worker reads a request in a single `socket.read`, which assumes it arrives
  in one segment — true for these short requests over USB-net, but a general
  server would loop until the header terminator.
- The socket budget is `POOL_MAX + 1` (`net::SOCKET_BUDGET`): one socket per http
  worker plus embassy-net's internal DNS slot. The OTA download needs no extra
  slot — the pool is drained before it opens its socket.
