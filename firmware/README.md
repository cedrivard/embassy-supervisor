# embassy-supervisor demo firmware (RP2350)

A complete embedded application demonstrating the
[`embassy-supervisor`](https://crates.io/crates/embassy-supervisor) crate driving a
real [embassy](https://embassy.dev) firmware. The supervisor is the star; this
firmware keeps each task thin (wrapping third-party crates) so the orchestration
stays in view.

`embassy-supervisor` provides, at runtime:

- **Dependency-ordered bring-up / teardown** (topological sort of a task graph).
- **Lifecycle modes** — `Terminate` (exit + respawn), `Pause` (park + resume,
  keeping a held resource), `OnDemand` (pool workers brought up/down with load).
- **Elastic task pools** — grow immediately under load, shrink after a cooldown.
- **Dependency- and pool-honoring control** — start/stop/pause/resume a task (or
  a whole pool) at runtime; a stop cascades through dependents, a start through
  dependencies, and a manual stop "sticks" against autoscaling.

This firmware exercises all of the above on an RP2350 — USB networking, an HTTP
control/observability plane, an elastic pool of keep-alive HTTP workers, a
Pause-mode heartbeat LED, and an OTA update path — each mapped to a supervisor
capability in the table below. It depends on the crate via a path dependency
aliased to `supervisor`, with the optional `defmt` feature enabled.

## Repository

- [`embassy-supervisor`](../supervisor) — the generalized, HAL-agnostic supervisor
  crate this firmware demonstrates. It has **zero board or application specifics**:
  it depends only on `embassy-executor`/`-sync`/`-time`/`-futures` +
  `heapless`/`portable-atomic` (and optional `defmt`), so it compiles for any
  embassy target.
- `firmware/` (this crate) — the RP2350 demo application; the only RP-specific code.
- `bootloader/` — the embassy-boot-rp A/B bootloader (the ROM boots it; it swaps
  DFU↔ACTIVE on a pending update).

## The supervised task graph

The firmware builds this graph (see `firmware/src/main.rs`):

```
net (Terminate) ─┬─ http pool (1 Terminate floor + 3 OnDemand)   heartbeat (Pause, standalone)
                 └─ ota (Terminate, disabled at boot; started via control)
```

The `http` pool is both the worker pool and the control/observability plane: each
worker is an HTTP/1.1 keep-alive server, and the pool grows under concurrent load.
`ota` is a stopped-at-boot node that, when started, drains the others to free heap
for the firmware download + decode (see [OTA update](#ota-update)).

| Supervisor feature      | Where                                                    |
|-------------------------|----------------------------------------------------------|
| Task dependencies        | `http`/`ota` depend on `net`; ordered start/teardown     |
| Dynamic task pools       | `http` `ElasticPool<DeferredShrink>` (`http.rs`)         |
| Resource: sockets        | the pool scales within the fixed `StackResources` budget |
| Resource: heap (budgeted)| `ota` drains `http` + `net` to free the arena for the decode |
| Lifecycle: Pause/Resume  | `heartbeat` keeps its LED `Output` across a pause         |
| Lifecycle: control-started| `ota` is a `Terminate` node pre-disabled at boot, started by control |
| Detached teardown        | `ota` outlives the `net` it tears down (`set_detached`)  |
| Runtime control          | `POST /api/control` → `supervisor::request_control`      |
| Observability            | `GET /api/tasks` (and the page at `/`)                    |

Networking is **USB-CDC-NCM** (TCP/IP over the USB cable). This is a deliberate
demo choice for **ease of reuse and testing** — it needs no extra hardware or
wireless setup and works on any embassy MCU with USB, so the supervisor stays the
focus. A real-world application would swap it for a wireless chip (Wi-Fi/BLE/
Thread); only the `net` task changes — the rest of the task graph is unaffected.

### Heap as a budgeted resource

`embedded-alloc` is the global allocator, and the networked subsystems' working
memory lives on it — so free heap (shown in the task view) **moves as the
supervisor starts and stops tasks**:

- **`net`** owns its *entire* USB + stack bring-up inside its own task: on start it
  heap-allocates all buffers (USB descriptors, the ~12 KB CDC-NCM packet pool, the
  socket storage); on stop it drops them, **returning net's whole budget** (see
  `net.rs`). So `net` is a genuinely reclaimable subsystem, not a fixed reservation.
- **`http`** — each worker allocates its I/O buffers on start and frees them on
  exit, so the pool's heap footprint tracks the live worker count (grow consumes,
  shrink returns).
- **`heartbeat`** allocates nothing on the heap; it only owns its LED `Output`.
- **`ota`** is the payoff: when started it drains the `http` pool **and** `net`
  to reclaim their budget, giving the zstd decoder nearly the whole arena, then
  resets. This is the supervisor's signature move — coordinated drain-to-free-budget
  for a disruptive operation (see [OTA update](#ota-update)).

Allocation is infallible-but-safe: the supervisor's start/stop is admission
control that keeps total usage within the arena, rather than per-allocation
fallibility. The arena is sized so the two big consumers — the serving pool and
the OTA decoder — peak at the same ~28 KB and never coexist.

## Memory footprint

Measured from the release binary (`thumbv8m.main-none-eabihf`, `opt-level="s"`,
fat LTO) plus exact `size_of` of the heap types. Reproduce with
`rust-size -A target/thumbv8m.main-none-eabihf/release/firmware` and
`rust-nm --print-size --size-sort`.

| Region | Used | Capacity | % |
|---|---:|---:|---:|
| **Flash** (`.text`+`.rodata`+`.data`) | ~159 KB | 892 KB (ACTIVE partition) | 17.8% |
| **Static RAM** (`.data`+`.bss`+`.uninit`) | ~41 KB | 512 KB | 8.0% |
| ↳ heap arena | 32 KB | — | — |
| ↳ everything else static | ~8.8 KB | — | — |
| **Max stack** (RAM left over) | ~471 KB | — | — |

Flash grew from ~87 KB (pre-OTA) to ~159 KB once OTA pulled in `reqwless`,
`ruzstd`, and `embassy-boot-rp`; it's bounded by the 892 KB ACTIVE partition, not
the full 2 MB. The compressed update image must also fit the 128 KB scratch region
and decode into the 896 KB DFU partition.

### Static RAM (compile-time, independent of runtime task state)

| Symbol | Bytes | What |
|---|---:|---|
| `heap::HEAP_MEM` | 32,768 | the heap arena (see below) |
| `net::net_task::POOL` | 3,728 | net worker future (mostly irreducible runner state) |
| `http::http_task::POOL` | 1,280 | `TaskStorage` for 4 http worker futures (~320 B each) |
| `defmt_rtt::BUFFER` | 1,024 | RTT log ring |
| `ota::ota_task::POOL` | 984 | OTA worker future (reqwless buffers are heap, not here) |
| wakers / nodes / channels / supervisor | ~1,200 | gpio+usb wakers, the `TaskNode`s, control mailbox, `HTTP_POOL`, … |

Non-heap static (~8.8 KB) is dominated by the task futures, which are **always
reserved** — net/http/ota futures exist even when those tasks are stopped; only the
*buffers* the tasks own are dynamic. The OTA future stays ~1 KB because reqwless's
socket/header/chunk buffers are heap-allocated rather than inlined; each http future
is ~320 B because response bodies are built on the heap, not in inline
`heapless::String`s across the write `.await`. `net`'s ~3.7 KB future is mostly
embassy/smoltcp runner state with no large app-owned local to box away.

### Heap (the 32 KB `embedded-alloc` arena)

| Subsystem | Allocation | Bytes | Freed when |
|---|---|---:|---|
| **net** | `NetState<1514,4,4>` packet pool | 12,264 | `net` stops |
| | `StackResources<5>` + dns socket | ~2,900 | `net` stops |
| | USB descriptors + control buf | 640 | `net` stops |
| | CDC-NCM `State` | 28 | `net` stops |
| | *net subtotal* | **~15,800** | |
| **http** (per worker) | `rx`+`tx`+`req` (3×1 KB `Vec`) | 3,072 | worker shrinks/stops |
| **http** (per request) | response body/header `String` | ~0.6–1 KB | request completes |
| **ota** (download) | reqwless `TcpClientState` + header/chunk buffers | ~4,600 | download finishes |
| **ota** (decode) | `ruzstd` window + literals/block + Huffman/FSE tables (`wlog=11`) | **27,615** | decode finishes |

| Scenario | Heap used | % of 32 KB | Free |
|---|---:|---:|---:|
| Steady (net + 1 http floor) | ~18.5 KB | 58% | ~13.5 KB |
| **Serving peak** (net + 4 http workers + 1 response) | **~27.4 KB** | 86% | ~4.6 KB |
| **OTA decode** (ruzstd alone — net + http both drained) | **~28 KB** | ~88% | ~4 KB |

The two peaks are **balanced by design at ~28 KB**, which is why 32 KB fits with
~4 KB margin:

- **Serving** is net + the 4-worker pool. The pool ceiling (`POOL_MAX`) is chosen so
  this matches the OTA peak — raising it raises both. Only one response `String` is
  ever live (small bodies fit the tx buffer, so the write never yields), as the
  Phase-1 build confirmed by measuring exactly 28,292 B at 4 workers.
- **OTA decode** is `ruzstd` alone: the `ota` task drains the http pool **and** `net`
  before decoding (it reads the staged image from flash, not the network). `ruzstd`'s
  footprint is the window + per-block literals/content buffers + ~18 KB of fixed
  Huffman/FSE decode tables; it ~doubles per `windowLog` (wlog 11→13 = 28→71 KB) while
  the compressed image barely shrinks, so the window is capped at **11**. The
  27,615 B figure is **measured** by the `zstd-heapcheck` tool decoding the real
  image, not estimated.

`GET /api/tasks` reports the true runtime `free_bytes()` (LlffHeap rounds each alloc
to an 8-byte block, so live usage is marginally above these requested bytes).

Two implementation notes. **Task futures are static, their buffers are heap** — so
stopping `net` frees ~16 KB of heap but not its 3.7 KB future slot. And
`Box::new(NetState::new())` has no *guaranteed* placement-new (the value is built as
a temporary, then moved into the heap), so a debug build copies the 12 KB `NetState`
through the bring-up poll's stack frame; the release build elides it (largest
task-poll frame is ~2.9 KB) — harmless either way against the ~471 KB stack.

## Build & run (RP2350)

Requires a debug probe + [`probe-rs`](https://probe.rs) (defmt logs stream over
the probe; the USB port is the network link).

The firmware always runs from the **ACTIVE partition under the bootloader** (it is
linked at `0x10021000` and cannot boot standalone), so a first flash installs the
**bootloader + firmware pair**. The bootloader arms an 8 s watchdog as its OTA
rollback safety; the firmware feeds it, so a healthy image stays up and a hung one
resets and rolls back.

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

### Host network setup (USB-net)

The device uses a static IP `10.42.0.61/24`. Point the host's USB ethernet
interface at the same subnet, then browse to the device:

```sh
ip addr add 10.42.0.1/24 dev usb0      # interface name varies (enxXX… on some hosts)
ip link set usb0 up
xdg-open http://10.42.0.61/             # task view + stop/start buttons
```

### Exercise the supervisor

- **Dynamic pool:** hold several concurrent connections open to the HTTP port so
  every worker is busy, and watch the pool grow `1 → 4` in the task view (free
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
- **Runtime parameter:** change the heartbeat without a rebuild —
  `?ms=` is `>0` blink half-period, `0` LED off, `<0` LED on, applied immediately:
  ```sh
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=100'   # fast blink
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=0'     # off
  curl -XPOST 'http://10.42.0.61/api/heartbeat?ms=-1'    # on
  ```

## Portability

The `embassy-supervisor` crate is HAL-agnostic and reused verbatim on any embassy
target. Porting this firmware to another MCU means swapping `embassy-rp` for the
target HAL, the USB init, and `embassy-boot-<mcu>` — the supervisor, USB-net,
HTTP plane, OTA flow, and the whole task graph stay.

## OTA update

A/B firmware update over USB-net with `embassy-boot-rp` rollback. `ota` is a
`Terminate` node pre-disabled at boot, so it sits stopped until control starts it.
Once started, **the node orchestrates its own resource draining** — the supervisor's
signature move:

1. `POST /api/ota[?ip=&port=&path=]` records a download target (each part defaults:
   gateway `10.42.0.1`, port `8000`, path `/fw.zst`) and issues `Activate(ota)`.
   Starting the node straight from the dashboard (or `/api/control?node=ota&op=start`)
   works too — with no target set, the task falls back to the same defaults.
2. The `ota` task **drains the http pool** (and waits via `is_running` for the
   workers' sockets to free), pulls the **zstd** image with `reqwless` over a
   socket it opens by IP, and streams it into a 128 KB **scratch** flash region.
3. It then **drains `net`** — it decodes from flash, not the network, so net's
   ~16 KB is reclaimed for the decoder. It `set_detached`es first so net's teardown
   doesn't cascade back into the still-running `ota` task.
4. `ruzstd` (`windowLog=11`) decodes the scratch image into the DFU partition with
   nearly the whole arena free, arms the swap (`mark_updated`), and resets.
5. The bootloader swaps DFU→ACTIVE; on the next boot the firmware feeds the
   watchdog and, once the network is up, calls `mark_booted` to confirm — otherwise
   the bootloader rolls back.

The window is capped at `wlog=11` because ruzstd's heap ~doubles per `windowLog`
while the compressed image barely shrinks; `tools`-style measurement of the real
image (`~/DEV/zstd-heapcheck`) put the decode peak at 27,615 B, which sizes the
32 KB arena.

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

## Notes

- `embassy-supervisor`'s logging is an optional `defmt` feature (no-op otherwise),
  which this firmware enables so the supervisor's lifecycle events show up over RTT.
- The HTTP plane is a small hand-rolled HTTP/1.1 handler on stable Rust, with
  keep-alive (one connection serves many requests; reaped on `Connection: close`
  or a 10 s idle timeout). `picoserve` was considered but its ergonomic embassy
  router needs the nightly `impl_trait_in_assoc_type` feature.
- Each worker reads a request in a single `socket.read`, which assumes it arrives
  in one segment — true for these short requests over USB-net, but a general
  server would loop until the header terminator.
