//! Executor spawner slots and resource slots.

use super::*;

// ─── Executor spawner slots ──────────────────────────────────────────────

/// Runtime-filled slot holding a foreign executor's [`SendSpawner`].
///
/// Declared by `executor NAME;` and filled by the app before or during
/// [`Supervisor::start`]. The supervisor waits for the slot to be filled
/// before spawning a node into it; if the slot is still empty after the
/// bounded wait, the spawn fails with [`FaultKind::ExecutorSlotEmpty`].
///
/// `default executor NAME;` makes `NAME` the default for nodes that do not
/// specify an executor. This lets a supervisor running on an interrupt tier
/// keep most of its graph in thread mode:
///
/// ```ignore
/// supervisor_graph! {
///     default executor THREAD;
///     executor HIGH;
///     node LOGGER  = Terminate, task: logger_worker;
///     node SAMPLER = Terminate, executor: HIGH, task: sampler_worker;
/// }
/// THREAD.set(spawner.make_send());
/// ```
///
/// `Send` is required on the spawn arguments, not the future. A `task:` shell's
/// arguments are always `Send`, so any `task:` worker can be routed to any tier.
/// A `spawn:` fn's own arguments must be `Send`, so this does not compile:
///
/// ```compile_fail
/// use std::rc::Rc;
/// use embassy_supervisor::{TaskNode, supervisor_graph};
///
/// #[embassy_executor::task]
/// async fn worker(_node: &'static TaskNode, _handle: Rc<u32>) {}
///
/// supervisor_graph! {
///     executor HIGH;
///     node A = Terminate, deps: [], executor: HIGH, spawn: worker(Rc::new(1));
/// }
/// ```
///
/// What the worker then touches from that tier is the author's contract.
///
/// # The executor fence
///
/// A `spawn:` task that acks its stop is still inside its final poll when the
/// supervisor — on a higher-priority tier, or another core — wakes for the ack,
/// and its task slot frees only once that poll returns. Spawning again then
/// fails `Busy`. The supervisor does not spin or sleep on it: it asks the slot's
/// **fence**, a resident task on the same executor, for one pass. An executor
/// polls one task at a time, so by the time the fence runs, the old task's poll
/// is over and its slot is free. `supervisor_graph!` gives every `executor NAME;`
/// a fence ([`with_fence`](Self::with_fence)); `set` spawns it onto the tier.
///
/// For a `spawn:` node this holds when the task fn has no free pool slot
/// (`pool_size = 1`, the default): the spawn then fails `Busy` until the
/// previous instance has returned. With a pooled task fn the spawn can succeed
/// while the old instance is still running, so such a body must make its ack
/// (`ack_dropped`, `mark_exited`, the `_acked` combinators) its last use of the
/// node: anything it does to the node afterwards lands on the new activation.
pub struct SpawnerSlot {
    slot: BlockingMutex<CriticalSectionRawMutex, Cell<Option<SendSpawner>>>,
    /// Wakes a `ready()` waiter when `set` fills the slot (cross-core safe:
    /// `Signal` is critical-section based and latches).
    filled: Signal<CriticalSectionRawMutex, ()>,
    /// Spawns this slot's fence onto the tier (`supervisor_graph!` emits it).
    fence: Option<FenceSpawn>,
    /// The fence task is spawned and parked on `fence_req`.
    fence_up: AtomicBool,
    /// Supervisor → fence: run one pass. Latching, so concurrent requests
    /// coalesce into one pass, which serves them all.
    fence_req: Signal<CriticalSectionRawMutex, ()>,
    /// Completed fence passes, wrapping. A waiter compares for inequality.
    passes: AtomicU16,
}

/// Spawns a [`SpawnerSlot`]'s executor fence onto the given spawner; see
/// [`SpawnerSlot::with_fence`].
pub type FenceSpawn = fn(SendSpawner, &'static SpawnerSlot) -> Result<(), SpawnError>;

impl SpawnerSlot {
    /// An empty slot (`const` — it lives in a `static` the macro emits).
    pub const fn new() -> Self {
        Self {
            slot: BlockingMutex::new(Cell::new(None)),
            filled: Signal::new(),
            fence: None,
            fence_up: AtomicBool::new(false),
            fence_req: Signal::new(),
            passes: AtomicU16::new(0),
        }
    }

    /// Give the slot an executor fence (see the type docs): `spawn` spawns
    /// a task on the given spawner that runs [`__sv_fence_loop`](Self::__sv_fence_loop)
    /// for this slot. Emitted by [`supervisor_graph!`] for every `executor NAME;`.
    /// A slot without one has no pass to offer: a `Busy` respawn then retries
    /// on the next gate event or at the deadline.
    pub const fn with_fence(mut self, spawn: FenceSpawn) -> Self {
        self.fence = Some(spawn);
        self
    }

    /// Fill the slot (last set wins), spawn its fence onto the tier on first
    /// fill, and wake a [`ready`](Self::ready) waiter and the supervisor's gate
    /// wait. Call before [`Supervisor::start`], or from the other core's
    /// bring-up while the supervisor waits for the slot.
    pub fn set(&'static self, spawner: SendSpawner) {
        self.slot.lock(|c| c.set(Some(spawner)));
        if let Some(spawn) = self.fence
            && !self.fence_up.swap(true, Ordering::AcqRel)
            && spawn(spawner, self).is_err()
        {
            warn!("supervisor: executor fence could not be spawned");
            self.fence_up.store(false, Ordering::Release);
        }
        self.filled.signal(());
        __sv_gate_event();
    }

    /// The registered spawner, or `None` while unfilled.
    pub fn get(&self) -> Option<SendSpawner> {
        self.slot.lock(Cell::get)
    }

    /// Await the slot and return the spawner: the app-side rendezvous, so a
    /// tier filled late, or from another core, is handled without a race.
    /// Returns immediately once the slot is filled, so any number of *late*
    /// callers are fine (an application can gate work on the executor being
    /// up). While the slot is still empty, park at most one task here: the
    /// `Signal` holds one waker, so two pre-fill waiters wake each other on
    /// every poll and only one of them sees the fill. The supervisor never
    /// parks here; it watches the shared gate event.
    pub async fn ready(&self) -> SendSpawner {
        loop {
            if let Some(sp) = self.get() {
                return sp;
            }
            // `Signal` latches: a `set()` racing between the check above and
            // this wait still wakes us.
            self.filled.wait().await;
        }
    }

    /// Ask the fence for one pass. `false` if this slot has no running fence.
    pub(crate) fn request_pass(&self) -> bool {
        if !self.fence_up.load(Ordering::Acquire) {
            return false;
        }
        self.fence_req.signal(());
        true
    }

    pub(crate) fn passes(&self) -> u16 {
        self.passes.load(Ordering::Acquire)
    }

    /// The fence task's body: wait for a request, count the pass, wake the
    /// supervisor. Being polled at all is the pass: every task polled before it
    /// on this executor has returned from its poll.
    #[doc(hidden)]
    pub async fn __sv_fence_loop(&self) -> ! {
        loop {
            self.fence_req.wait().await;
            self.passes.fetch_add(1, Ordering::AcqRel);
            __sv_gate_event();
        }
    }
}

impl Default for SpawnerSlot {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ResourceSlot ────────────────────────────────────────────────────────

/// Type-erased readiness view of a [`ResourceSlot`], for the supervisor's
/// bring-up wait.
///
/// A `TaskNode` can gate on any number of slots of *different* `T`s, so the node
/// stores `&'static [&'static dyn ResourceGate]` (object-safe: no `T` in the
/// signatures). Same shape as embassy's `dyn` driver registries — see
/// <https://doc.rust-lang.org/reference/items/traits.html#object-safety>.
/// The supervisor only needs "is it filled?", re-checked on the shared gate
/// event every fill signals; taking the value stays in the generated spawn
/// glue, where the concrete `T` is known.
pub trait ResourceGate: Sync {
    /// Non-consuming "is the slot currently filled" check.
    fn is_filled(&self) -> bool;
    /// Empty the slot, dropping any held value — how a stopping provider's
    /// `provides:` list is cleared, so a value that dies with its producer
    /// reads empty until the next activation re-provides. Default no-op, for a
    /// gate view with nothing to clear.
    fn clear(&self) {}
}

/// A one-value handoff cell threading an owned resource from `main` into a
/// supervised task — the safe replacement for `Peripherals::steal()` inside
/// the task body.
///
/// Declared (as a `pub static`) by [`supervisor_graph!`] for each entry in a
/// node's `resources:` clause. The protocol:
///
/// 1. `main` splits `Peripherals` and **moves** the resource in with
///    [`provide`](Self::provide). This is where the compile-time guarantee
///    lives: the singleton field is *consumed*, so no second owner — and no
///    `unsafe` steal — can exist.
/// 2. The bring-up awaits the slot being filled, then the generated task
///    shell [`take`](Self::take)s it — inside the spawned task, so a value
///    never crosses the spawn call. A slot still empty at the gate deadline
///    fails the spawn with [`FaultKind::ResourceMissing`] — a fail-closed
///    error out of [`Supervisor::start`], not a panic inside the task
///    (compare `static_cell::StaticCell`, which panics on misuse).
/// 3. The generated task shell hands the worker `&mut T` and
///    [`restore`](Self::restore)s the value after the worker returns, so a
///    `Terminate` respawn re-takes the *same instance* instead of stealing a
///    fresh one. (A `Pause` worker never returns — it parks — so it simply
///    retains the resource, exactly like a hand-written parked task.)
///
/// Same primitives as [`SpawnerSlot`]: a critical-section
/// [`BlockingMutex`]`<`[`Cell`]`<Option<T>>>` for the value (`Sync` for
/// `T: Send`, provided by embassy-sync — no `unsafe` here) plus a latching
/// [`Signal`] for one app-side waiter ([`wait_take`](Self::wait_take)). The
/// supervisor awaits late provisioning (bounded; see [`Supervisor::start`])
/// on the shared gate event instead.
pub struct ResourceSlot<T> {
    slot: BlockingMutex<CriticalSectionRawMutex, Cell<Option<T>>>,
    /// Wakes a [`wait_take`](Self::wait_take) waiter when `provide`/`restore`
    /// fills the slot (latching, so a fill racing the check-then-wait still
    /// wakes it).
    filled: Signal<CriticalSectionRawMutex, ()>,
}

impl<T> ResourceSlot<T> {
    /// An empty slot (`const` — it lives in a `static` the macro emits).
    pub const fn new() -> Self {
        Self {
            slot: BlockingMutex::new(Cell::new(None)),
            filled: Signal::new(),
        }
    }

    /// Move the resource in (from `main`'s `Peripherals` split) and wake the
    /// supervisor's pre-spawn wait. Call before [`Supervisor::start`]; a slot
    /// still empty after the supervisor's bounded wait fails that node's spawn
    /// with [`FaultKind::ResourceMissing`]. Filling an occupied slot replaces
    /// (drops) the old value, so don't: one resource, one slot, moved exactly once.
    pub fn provide(&self, value: T) {
        // Drop a replaced value outside the critical section.
        let old = self.slot.lock(|c| c.replace(Some(value)));
        drop(old);
        self.filled.signal(());
        __sv_gate_event();
    }

    /// Take the resource out, leaving the slot empty. Called by the generated
    /// task shell at the start of each activation; `None` means "not provided
    /// yet" or "currently held by a live task instance".
    #[must_use = "dropping the value loses the resource for good"]
    pub fn take(&self) -> Option<T> {
        self.slot.lock(Cell::take)
    }

    /// Copy the resource out **without emptying the slot** — the `shared`
    /// resource kind's read: any number of consumers (several nodes, a whole
    /// pool) get the same `Copy` handle, and the slot stays filled for the
    /// next one. Only for `T: Copy` (a `Stack`-like handle, a `&'static`
    /// registry ref); an owned singleton uses [`take`](Self::take).
    pub fn get(&self) -> Option<T>
    where
        T: Copy,
    {
        self.slot.lock(Cell::get)
    }

    /// Put the resource back for the next spawn. Called by the generated task
    /// shell after the worker returns (i.e. after its clean shutdown ack), so a
    /// respawn re-takes the same instance.
    pub fn restore(&self, value: T) {
        self.provide(value);
    }

    /// Empty the slot, dropping any held value, and reset the latched filled
    /// signal. The provider side of the freshness convention: a task that
    /// rebuilds this slot's value each activation clears it on the way down —
    /// the `provides:` clause does it from the shutdown ack — so a consumer's
    /// gate wait holds for the next activation's value instead of taking this
    /// one's leftover.
    pub fn clear(&self) {
        let stale = self.slot.lock(Cell::take);
        drop(stale);
        self.filled.reset();
    }

    /// Await the slot being filled, then take the value — how an application
    /// reads a node's `exit:` slot (the shell `provide()`s the worker's return
    /// value there just before recording the exit). Check-then-park, so a value
    /// provided earlier is returned immediately; the latching signal carries
    /// the same single-pre-fill-waiter caveat as [`SpawnerSlot::ready`], so N
    /// concurrent readers fan out through an app-owned `Watch` instead. The
    /// supervisor never parks here; it watches the shared gate event.
    pub async fn wait_take(&self) -> T {
        loop {
            if let Some(v) = self.take() {
                return v;
            }
            self.filled.wait().await;
        }
    }
}

// `T: Send` (not just any `T`): the gate is reachable from the supervisor task,
// which may run on a different core than the provider — the same bound the
// inner `BlockingMutex` requires for `Sync`, restated here so the `dyn` upcast
// can't outrun it.
impl<T: Send> ResourceGate for ResourceSlot<T> {
    fn is_filled(&self) -> bool {
        // Peek without consuming: `Cell` has no `&T` access (no `T: Copy`
        // here), so take-and-put-back under the same critical section.
        self.slot.lock(|c| {
            let v = c.take();
            let filled = v.is_some();
            c.set(v);
            filled
        })
    }

    fn clear(&self) {
        ResourceSlot::clear(self);
    }
}

impl<T> Default for ResourceSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}
