//! The supervisor: bring-up and teardown waves, the single-node verbs and
//! the driver loop.

use super::*;

/// Retry cadence for a spawn that stays `Busy` after one executor pass:
/// an exhausted task pool or a refused `state:` allocation is not cured by
/// yielding, so the retry sleeps instead of spinning.
const BUSY_BACKOFF: embassy_time::Duration = embassy_time::Duration::from_millis(1);

// ─── Supervisor ──────────────────────────────────────────────────────────

/// What woke the driver loop.
#[cfg(any(feature = "pool", feature = "control"))]
enum DriverWake {
    #[cfg(feature = "control")]
    Control(ControlCommand),
    #[cfg(feature = "bound-deps")]
    Bind,
    #[cfg(feature = "pool")]
    Pools,
}

/// Orchestrates a set of managed tasks across spawn / teardown / bring-up.
///
/// Built from a [`Graph`] with [`Supervisor::new`] and owned by one supervisor
/// task: that task is the only caller of the verbs (`start`, `run`,
/// `teardown`, `activate`, `restart` and the rest), which never run
/// concurrently with each other. Other tasks reach it through the control
/// mailbox ([`request_control`]) or through each [`TaskNode`]'s own atomic
/// state, never through the `Supervisor` struct. It copies nothing out of the
/// graph but references, so building one is free.
pub struct Supervisor<const N: usize, T: Topology<N> = Ordered<N, { shape::ALL }>> {
    /// Node slots, one per declared node. `None` marks a slot whose node was
    /// `#[cfg]`-ed out of the build (feature-gated); every method skips those.
    nodes: &'static [Option<&'static TaskNode>; N],
    /// The graph's [`Topology`]: the dep rows and topological order every walk
    /// iterates (reverse iteration is the teardown order), and the structural
    /// [`shape`] bits the gates fold on. Borrowed from the `static` [`Graph`]
    /// rather than copied: a `Supervisor` usually lives inside a task future
    /// (i.e. in that task's `static` storage), so an inline order array would
    /// cost N bytes of RAM per supervisor plus the copy code for no benefit —
    /// and for [`Flat`] this reference is the field's entire cost.
    topo: &'static T,
    /// Elastic pools, so the control interface can co-control a whole pool from
    /// any one member (`apply_control` expands the target through
    /// [`Pool::members`]) — the same registry `run_pools` drives. Taken from
    /// `GRAPH.pools` at construction (empty when no pool is declared).
    #[cfg(feature = "pool")]
    pub(crate) pools: &'static [&'static dyn Pool],
    /// This graph as one `'static`, linked into the binary-wide chain by
    /// [`start`](Supervisor::start) so the trace hooks can resolve a task id to
    /// one of its nodes.
    #[cfg(feature = "trace")]
    graph_ref: &'static GraphRef,
}

/// Wait until `cond` holds or `deadline` passes, re-checking on every gate
/// event (slot fills, readiness, a resident shell parking, a fence pass).
/// Returns the final `cond`.
async fn wait_gate_until(deadline: embassy_time::Instant, mut cond: impl FnMut() -> bool) -> bool {
    loop {
        let seen = GATE_EVT.generation();
        if cond() {
            return true;
        }
        if embassy_time::Instant::now() >= deadline {
            return false;
        }
        let _ = select(GATE_EVT.wait_past(seen), Timer::at(deadline)).await;
    }
}

/// `d` in whole milliseconds, rounded up: a wave budget stamped mid-millisecond
/// then ends at or after its real deadline, never before.
pub(crate) const fn ceil_ms(d: embassy_time::Duration) -> u64 {
    let ms = d.as_millis();
    if embassy_time::Duration::from_millis(ms).as_ticks() < d.as_ticks() {
        ms + 1
    } else {
        ms
    }
}

/// One pass of the executor `node` spawns on, after a `Busy` spawn: its slot's
/// fence when it has one, else — the node spawns on the supervisor's own
/// executor — a yield, which lets every queued task finish its poll first. A
/// slot without a fence has no pass to offer; the retry then waits for the
/// next gate event or the deadline.
async fn executor_pass(node: &'static TaskNode, deadline: embassy_time::Instant) {
    match node.cfg.spawn_slot {
        Some(slot) => {
            // Snapshot before requesting: a fence on another core may finish
            // the pass before we read `passes` again.
            let seen = slot.passes();
            let gate = GATE_EVT.generation();
            if slot.request_pass() {
                wait_gate_until(deadline, || slot.passes() != seen).await;
            } else {
                let _ = select(GATE_EVT.wait_past(gate), Timer::at(deadline)).await;
            }
        }
        None => embassy_futures::yield_now().await,
    }
}

/// Spawn `node` once its previous instance has let go: a resident shell is
/// awaited until it parks (`is_settled`), and a `spawn:` task still inside
/// its final poll is waited out with one pass of its executor for the
/// first `Busy` (its fence, see [`SpawnerSlot`], or a yield on the
/// supervisor's own executor), then a 1 ms backoff per further `Busy`.
/// Bounded by `deadline`. The reset and the activation clears sit
/// immediately before each attempt, with no await in between, so nothing the
/// previous instance wrote late survives into the new one.
///
/// For a `spawn:` node this holds when the task fn has no free pool slot
/// (`pool_size = 1`, the default): the spawn then fails `Busy` until the
/// previous instance has returned. With a pooled task fn the spawn can succeed
/// while the old instance is still running, so such a body must make its ack
/// (`ack_dropped`, `mark_exited`, the `_acked` combinators) its last use of the
/// node: anything it does to the node afterwards lands on the new activation.
///
/// A free fn, not a `Supervisor` method: it never touches the graph, so one
/// copy serves every graph type.
async fn spawn_settled(
    node: &'static TaskNode,
    spawn: fn(Spawner) -> Result<(), SpawnError>,
    spawner: &Spawner,
    deadline: embassy_time::Instant,
) -> Result<(), SpawnError> {
    if !wait_gate_until(deadline, || node.is_settled()).await {
        return Err(SpawnError::Busy);
    }
    let mut passed = false;
    loop {
        let seen = GATE_EVT.generation();
        node.arm();
        let err = match spawn(*spawner) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        let now = embassy_time::Instant::now();
        if now >= deadline {
            return Err(err);
        }
        if !passed {
            passed = true;
            executor_pass(node, deadline).await;
        } else {
            let until = deadline.min(now + BUSY_BACKOFF);
            let _ = select(GATE_EVT.wait_past(seen), Timer::at(until)).await;
        }
    }
}

/// Await the node's `executor:` slot being filled. Bounded by the node's
/// `slot_timeout`; waits on the shared gate event, never on the slot's
/// single-waiter `Signal`, so an app task parked on that `Signal` neither
/// delays the supervisor nor is woken by it.
async fn await_spawn_slot(node: &'static TaskNode) -> Result<(), NodeFault> {
    let Some(slot) = node.cfg.spawn_slot else {
        return Ok(());
    };
    let deadline = embassy_time::Instant::now() + node.slot_timeout();
    if wait_gate_until(deadline, || slot.get().is_some()).await {
        Ok(())
    } else {
        Err(NodeFault {
            node,
            kind: FaultKind::ExecutorSlotEmpty,
        })
    }
}

/// Await every [`ResourceSlot`] a node's `resources:` clause takes from being
/// filled. Each gate is bounded by the node's `slot_timeout`; waits on the
/// shared gate event, never on the slot's single-waiter `Signal`, so an app
/// task parked on that `Signal` neither delays the supervisor nor is woken by
/// it.
async fn await_resources(node: &'static TaskNode) -> Result<(), NodeFault> {
    for gate in node.cfg.resource_gates {
        let deadline = embassy_time::Instant::now() + node.slot_timeout();
        if !wait_gate_until(deadline, || gate.is_filled()).await {
            return Err(NodeFault {
                node,
                kind: FaultKind::ResourceMissing,
            });
        }
    }
    Ok(())
}

/// Await every `ready`-marked dep's task-asserted readiness before spawning.
/// Each dep is bounded by the node's `slot_timeout`; waits on the shared gate
/// event, never on the dep's single-waiter `Signal`, so an app task parked on
/// that `Signal` neither delays the supervisor nor is woken by it.
#[cfg(feature = "readiness")]
async fn await_ready_deps(node: &'static TaskNode) -> Result<(), NodeFault> {
    for dep in node.ready_deps() {
        let deadline = embassy_time::Instant::now() + node.slot_timeout();
        if !wait_gate_until(deadline, || dep.is_ready()).await {
            return Err(NodeFault {
                node,
                kind: FaultKind::ReadyDepTimeout { dep },
            });
        }
    }
    Ok(())
}
#[cfg(not(feature = "readiness"))]
async fn await_ready_deps(_node: &'static TaskNode) -> Result<(), NodeFault> {
    Ok(())
}

impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    const NODE_CAP: () = assert!(N <= 256, "supervisor: a graph holds at most 256 nodes");

    /// Create a supervisor from a statically-built graph.
    pub const fn new(graph: &'static Graph<N, T>) -> Self {
        let () = Self::NODE_CAP;
        Self {
            nodes: graph.nodes,
            topo: &graph.topo,
            #[cfg(feature = "pool")]
            pools: graph.pools,
            #[cfg(feature = "trace")]
            graph_ref: graph.graph_ref,
        }
    }

    /// Does this graph's shape carry `bit` (see [`shape`])? `T::SHAPE` is an
    /// associated const, so every call folds to a constant.
    #[inline(always)]
    pub(crate) fn has(bit: u32) -> bool {
        has(T::SHAPE, bit)
    }

    fn order_iter(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        (0..N).map(|k| self.topo.order_at(k) as usize)
    }

    /// Slot of `node` in this supervisor's graph (pointer identity).
    #[cfg(any(feature = "control", feature = "pool", feature = "breadcrumb"))]
    fn index_of(&self, node: &'static TaskNode) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.is_some_and(|x| core::ptr::eq(x, node)))
    }

    /// [`note`] for a single-node verb, which has the node but not its slot.
    #[inline(always)]
    fn note_verb(&self, node: &'static TaskNode, t: Transition) {
        #[cfg(feature = "breadcrumb")]
        let slot = self.index_of(node);
        #[cfg(not(feature = "breadcrumb"))]
        let slot = None;
        note(node, slot, t);
    }

    /// The pre-spawn gate sequence (executor slot, resource gates, `ready` deps),
    /// each wait compiled out when the graph's shape lacks the structure it serves.
    async fn await_gates(node: &'static TaskNode) -> Result<(), NodeFault> {
        if Self::has(shape::EXEC_SLOTS) {
            await_spawn_slot(node).await?;
        }
        if Self::has(shape::RESOURCES) {
            await_resources(node).await?;
        }
        if Self::has(shape::READY_DEPS) {
            await_ready_deps(node).await?;
        }
        Ok(())
    }

    /// Bring the graph from any quiescent state to running, in dependency
    /// order: cold boot and re-entry (a sub-graph supervisor is legitimately
    /// `start()`/`teardown()`-cycled per app phase). Idempotent: running nodes
    /// are skipped; detached nodes are skipped on re-entry (their instance
    /// survived the teardown; the first start still spawns them, the flag is
    /// app-set afterwards); a `Pause` instance parked by an earlier teardown is
    /// resumed in place, never double-spawned: it skips the executor-slot and
    /// resource gates (it kept its resources), and waits for its `ready` deps
    /// like any other node. `Mode::OnDemand` nodes are skipped: their pool's
    /// scaling or an explicit [`start_node`](Self::start_node) brings them up.
    ///
    /// # Panics
    /// Under `trace-self`, panics when polled outside an embassy executor.
    pub async fn start(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        self.bring_up(spawner, false, &mut KeepFirst, Origin::Boot)
            .await
    }

    /// The body of [`start`](Self::start). `keep_going` makes the wave
    /// best-effort: every node that can come up does, every fault goes to
    /// `policy`, and the first one it takes is returned once the wave has
    /// settled.
    async fn bring_up(
        &self,
        spawner: &Spawner,
        keep_going: bool,
        policy: &mut dyn RunPolicy,
        origin: Origin,
    ) -> Result<(), NodeFault> {
        #[cfg(feature = "trace")]
        self.graph_ref.register();

        #[cfg(feature = "trace-self")]
        if let Some(node) = self.graph_ref.self_node() {
            node.set_task_id(trace::current_task_id().await);
            node.handle.flag_set(flag::RUNNING | flag::DETACHED);
        }

        // `enabled_if:` decides whether a down node comes up; a running or
        // self-managed instance is left alone (and unlatched).
        #[cfg(feature = "enabled-if")]
        for node in self.nodes.iter().flatten() {
            if let Some(enabled) = node.cfg.enabled_if
                && !node.is_running()
                && !node.is_detached()
                && !enabled()
            {
                info!("supervisor: {} disabled by enabled_if", node.name());
                node.set_disabled(true);
            }
        }

        self.start_nodes(
            spawner,
            &mut |_, node| {
                !(Self::has(shape::ON_DEMAND) && matches!(node.mode(), Mode::OnDemand))
                    && !node.is_disabled()
                    && !node.is_collateral()
                    && !node.is_running()
                    && !node.is_detached()
            },
            keep_going,
            policy,
            origin,
        )
        .await
    }

    #[cfg(any(feature = "pool", feature = "control"))]
    /// Bring the graph up, then drive it: one loop waits for a pool scale
    /// request or policy deadline (`pool`), a control command (`control`) or a
    /// bound readiness change (`bound-deps`), and applies each to completion
    /// before it waits again, so two lifecycle waves never drive this graph at
    /// once. With `liveness-monitor` the [`monitor`](Self::monitor) runs from
    /// the first poll, alongside the boot wave, so a `ready_on_write` provider
    /// can assert readiness for its dependents' bring-up gates.
    ///
    /// The boot wave is best-effort: a node whose gate budget expires is logged
    /// with `warn!`, reported as `LifecycleKind::Faulted` (with lifecycle
    /// events), and left down and enabled, while the rest of the graph comes
    /// up. A failed control `Activate` and a failed pool growth are handled the
    /// same way. `run` returns only for a missed shutdown ack
    /// ([`FaultKind::ShutdownTimeout`]) or, with `restart-intensity`, a refused
    /// restart (`FaultKind::RestartStorm`): the [`ReturnOnHalt`] policy.
    /// [`run_with`](Self::run_with) picks another one.
    /// Re-entering `run` after a return is safe and retries every enabled node
    /// that is down.
    pub async fn run(&self, spawner: &Spawner) -> NodeFault {
        self.run_with(spawner, &mut ReturnOnHalt).await
    }

    #[cfg(any(feature = "pool", feature = "control"))]
    /// [`run`](Self::run) with a [`RunPolicy`] that picks the faults it
    /// returns.
    ///
    /// The policy is called once for every fault the driver meets, in the
    /// order they happen, with the [`Origin`] that raised it: the boot wave, a
    /// control command, a bind cascade or a pool pass. `true` makes `run_with`
    /// return that fault. `false` keeps the driver going. Either way the fault
    /// was already logged and reported as `Faulted`.
    ///
    /// - A best-effort wave (the boot wave, `Activate`, the way back up of a
    ///   `Restart`) reports every fault and runs to the end. `run_with` then
    ///   returns the first fault the policy took. The faults after it still
    ///   go to the policy, the log and the lifecycle events. A policy that
    ///   keeps them hands them to the caller after `run_with` returns.
    /// - A fault that stops its step (a missed ack, a refused restart, a pool
    ///   growth) ends that step whatever the policy says. A taken fault ends
    ///   `run_with` too.
    /// - An absorbed `ShutdownTimeout` leaves the wedged node marked running
    ///   until it acks. Only the log and the lifecycle events show it.
    ///
    /// The policy runs inside the wave, between two awaits. It must only
    /// observe: to act on a fault, send a command with
    /// [`try_request_control`], which the driver applies after the wave.
    ///
    /// Re-entering `run_with` after a return is safe and retries every enabled
    /// node that is down. With a policy that returns bring-up faults, a node
    /// that keeps faulting is retried once per `slot_timeout` by such a loop.
    ///
    /// ```ignore
    /// let mut policy = FailFast;
    /// loop {
    ///     let fault = SUP.run_with(&spawner, &mut policy).await;
    ///     escalate(fault).await;
    /// }
    /// ```
    pub async fn run_with(&self, spawner: &Spawner, policy: &mut dyn RunPolicy) -> NodeFault {
        let drive = async {
            if let Err(e) = self.bring_up(spawner, true, policy, Origin::Boot).await {
                return e;
            }
            self.run_driver(spawner, policy).await
        };
        #[cfg(feature = "liveness-monitor")]
        return match select(drive, self.monitor()).await {
            embassy_futures::select::Either::First(e) => e,
            embassy_futures::select::Either::Second(never) => match never {},
        };
        #[cfg(not(feature = "liveness-monitor"))]
        drive.await
    }

    /// The driver loop: a pools pass, then the next wake, applied to
    /// completion before the next wait. Only the waits race, so a command never
    /// cancels a pool stop in progress and a bound stop never overlaps another
    /// wave on the same node.
    #[cfg(any(feature = "pool", feature = "control"))]
    async fn run_driver(&self, spawner: &Spawner, policy: &mut dyn RunPolicy) -> NodeFault {
        use embassy_futures::select::{Either3, select3};
        loop {
            #[cfg(feature = "pool")]
            let deadline = if Self::has(shape::POOLS) {
                match self.pool_pass_with(spawner, policy).await {
                    Ok(next) => next,
                    Err(e) => return e,
                }
            } else {
                None
            };
            #[cfg(feature = "bound-deps")]
            let bind = async {
                if !Self::has(shape::BOUND_DEPS) {
                    core::future::pending::<()>().await;
                }
                wait_bind().await;
                DriverWake::Bind
            };
            #[cfg(not(feature = "bound-deps"))]
            let bind = core::future::pending::<DriverWake>();
            #[cfg(feature = "control")]
            let control = async { DriverWake::Control(wait_control().await) };
            #[cfg(not(feature = "control"))]
            let control = core::future::pending::<DriverWake>();
            #[cfg(feature = "pool")]
            let pools = async {
                if !Self::has(shape::POOLS) {
                    core::future::pending::<()>().await;
                }
                select(wait_scale(), pool::deadline_timer(deadline)).await;
                DriverWake::Pools
            };
            #[cfg(not(feature = "pool"))]
            let pools = core::future::pending::<DriverWake>();
            let wake = match select3(bind, control, pools).await {
                Either3::First(w) | Either3::Second(w) | Either3::Third(w) => w,
            };
            // Every fault went to `policy` where it was raised; an `Err` here
            // is one the policy took.
            let applied: Result<(), NodeFault> = match wake {
                // `select3` polls the bind wait first, and a flapping provider
                // re-latches it during every cascade: one queued command gets
                // its turn after each, so bind changes cannot starve control.
                #[cfg(feature = "bound-deps")]
                DriverWake::Bind => match self.apply_bind_with(spawner, policy).await {
                    Ok(()) => match CONTROL_REQ.try_receive() {
                        Ok(cmd) => self.drive_control(cmd, spawner, policy).await,
                        Err(_) => Ok(()),
                    },
                    e => e,
                },
                #[cfg(feature = "control")]
                DriverWake::Control(cmd) => self.drive_control(cmd, spawner, policy).await,
                #[cfg(feature = "pool")]
                DriverWake::Pools => Ok(()),
            };
            if let Err(e) = applied {
                return e;
            }
        }
    }

    /// Apply a command the driver received, reporting each fault. A command
    /// for a node outside this graph is logged and dropped;
    /// [`apply_control`](Self::apply_control) itself treats one as a silent
    /// no-op.
    #[cfg(feature = "control")]
    async fn drive_control(
        &self,
        cmd: ControlCommand,
        spawner: &Spawner,
        policy: &mut dyn RunPolicy,
    ) -> Result<(), NodeFault> {
        if self.index_of(cmd.node).is_none() {
            warn!(
                "supervisor: control command for {} ignored: not a node of this graph",
                cmd.node.name()
            );
            return Ok(());
        }
        match cmd.op {
            ControlOp::Deactivate => self.deactivate_with(cmd.node, policy).await,
            ControlOp::Activate => self.activate_with(cmd.node, spawner, policy).await,
            #[cfg(feature = "restart")]
            ControlOp::Restart => self.restart_with(cmd.node, spawner, policy).await,
        }
    }

    #[cfg(feature = "liveness-monitor")]
    /// Police the beats of every node that declares `beat_timeout:` and report
    /// through [`wait_health`]: a [`HealthKind::Stale`] once a running node
    /// misses `beat_window:` consecutive checks, a [`HealthKind::Recovered`]
    /// when it beats again. Report-only: it never stops, restarts or faults a
    /// node. Parks forever when no node declares `beat_timeout:`.
    ///
    /// To make a hardware watchdog honour the same policing, gate its feed on
    /// [`Graph::all_fresh`] rather than feeding unconditionally.
    pub async fn monitor(&self) -> core::convert::Infallible {
        if !Self::has(shape::BEATS)
            || !self
                .nodes
                .iter()
                .flatten()
                .any(|n| n.beat_timeout().is_some())
        {
            info!("supervisor: liveness monitor idle (no node declares beat_timeout)");
            core::future::pending::<()>().await;
        }

        loop {
            #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
            let seen = PROBE_EVT.generation();
            let sleep = self
                .nodes
                .iter()
                .flatten()
                .filter_map(|n| n.ticks_until_check())
                .min()
                .unwrap_or(1);
            let timer = Timer::after(embassy_time::Duration::from_ticks(sleep));
            // A `ready_on_write` node that starts while this sleeps re-plans
            // without a sweep, so its probe begins now and no strike is added.
            #[cfg(all(feature = "coupling-observe", feature = "readiness"))]
            if let embassy_futures::select::Either::Second(()) =
                select(timer, PROBE_EVT.wait_past(seen)).await
            {
                continue;
            }
            #[cfg(not(all(feature = "coupling-observe", feature = "readiness")))]
            timer.await;

            let observed = Self::has(shape::OBSERVED);
            for node in self.nodes.iter().flatten() {
                sweep_node(node, observed);
            }
        }
    }

    /// Start a single node: its gates, then its spawn. A node that is already
    /// running, or detached (the application's to manage), is left untouched
    /// and `Ok(())` is returned. A disabled or collateral node is started
    /// anyway: the explicit call overrides the hold.
    pub async fn start_node(
        &self,
        node: &'static TaskNode,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        if node.is_running() || node.is_detached() {
            return Ok(());
        }
        node.handle.flag_clear(flag::COLLATERAL);
        self.note_verb(node, Transition::Spawning);
        if let Some(spawn) = node.cfg.spawn {
            match Self::await_gates(node).await {
                Ok(()) => {}
                Err(_fault) => {
                    #[cfg(feature = "bound-deps")]
                    if let FaultKind::ReadyDepTimeout { dep } = _fault.kind
                        && node.bound_deps().iter().any(|b| core::ptr::eq(*b, dep))
                    {
                        info!(
                            "supervisor: {} parked, bound dep {} not ready",
                            node.name(),
                            dep.name()
                        );
                        node.handle.flag_set(flag::BOUND_STOPPED);
                        return Ok(());
                    }
                    self.note_verb(node, Transition::Faulted(_fault.kind));
                    return Err(_fault);
                }
            }
            let deadline = embassy_time::Instant::now() + node.slot_timeout();
            if let Err(err) = spawn_settled(node, spawn, spawner, deadline).await {
                let kind = FaultKind::Spawn(err);
                self.note_verb(node, Transition::Faulted(kind));
                return Err(NodeFault { node, kind });
            }
        } else {
            node.arm();
        }
        node.mark_running();
        self.note_verb(node, Transition::Started);
        info!("supervisor: started {}", node.name());
        Ok(())
    }

    async fn shutdown_and_wait(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        node.signal_shutdown();
        self.note_verb(node, Transition::StopRequested);
        self.await_ack(node).await
    }

    /// The waiting half of [`shutdown_and_wait`](Self::shutdown_and_wait), for
    /// a caller that signalled the node earlier. Returns immediately for a node
    /// that already acked.
    async fn await_ack(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        let deadline = embassy_time::Instant::now() + node.ack_timeout();
        loop {
            let seen = STOP_EVT.generation();
            if node.has_dropped() {
                break;
            }
            if embassy_time::Instant::now() >= deadline {
                warn!(
                    "supervisor: task {} did not ack shutdown within {}ms",
                    node.name(),
                    node.cfg.ack_timeout_ms,
                );
                #[cfg(feature = "budget")]
                if Self::has(shape::CLAIMS) {
                    node.release_claims();
                }
                self.note_verb(node, Transition::Faulted(FaultKind::ShutdownTimeout));
                return Err(NodeFault {
                    node,
                    kind: FaultKind::ShutdownTimeout,
                });
            }
            let _ = select(STOP_EVT.wait_past(seen), Timer::at(deadline)).await;
        }
        self.note_verb(node, acked(node));
        Ok(())
    }

    async fn stop_nodes(
        &self,
        select: &mut dyn FnMut(usize, &'static TaskNode) -> bool,
        keep_going: bool,
    ) -> Result<(), NodeFault> {
        let mut chosen = [false; N];
        for j in self.order_iter().rev() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if select(j, node) {
                chosen[j] = true;
            }
        }

        self.stop_wave(&chosen, keep_going).await
    }

    /// The down direction: signal each chosen node the moment every *chosen
    /// dependent* of it has resolved, parking on the ack event ([`STOP_EVT`])
    /// between rounds. Two guarantees at once, which signalling everything up
    /// front could not give together:
    ///
    ///   * a `deps:` dependency keeps serving until its stopping dependents
    ///     have acked, so a dependent may flush over a link, or drive one
    ///     last ioctl through a runner it depends on, during its own
    ///     shutdown;
    ///   * a node whose shutdown waits on a node it has NO edge to (a
    ///     producer draining a [`Leased`] signal) is never kept waiting on
    ///     the wave's own progress: every node without an unresolved chosen
    ///     dependent is signalled in the same round, so the holders it waits
    ///     on are already stopping.
    async fn stop_wave(&self, chosen: &[bool; N], keep_going: bool) -> Result<(), NodeFault> {
        const UNSIGNALED: u32 = u32::MAX;
        let epoch = embassy_time::Instant::now();
        let mut resolved = [false; N];
        let mut signaled: [u32; N] = [UNSIGNALED; N];
        // How many entries of unresolved chosen dependents' rows still name
        // each node: it is signalled once its count reaches zero. Counted once
        // and released as dependents resolve, so a wave costs O(N + E), not a
        // rescan of every row per node per round. `u16`, not `u8`: a row may
        // repeat a dep (two true cfgs), so a count can pass the dependent count.
        let mut holders = [0u16; N];
        for k in 0..N {
            if chosen[k] {
                for &d in self.topo.deps_of(k as u8) {
                    if chosen[d as usize] {
                        holders[d as usize] += 1;
                    }
                }
            }
        }
        let mut first_err = Ok(());
        loop {
            let seen = STOP_EVT.generation();
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] || signaled[j] != UNSIGNALED || holders[j] != 0 {
                    continue;
                }
                let node = self.nodes[j].expect("a chosen slot is occupied");
                node.signal_shutdown();
                note(node, Some(j), Transition::StopRequested);
                signaled[j] =
                    ceil_ms(embassy_time::Instant::now() - epoch).min(UNSIGNALED as u64 - 1) as u32;
            }

            let mut progress = false;
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] {
                    continue;
                }
                let node = self.nodes[j].expect("a chosen slot is occupied");
                if node.has_dropped() {
                    note(node, Some(j), acked(node));
                    resolved[j] = true;
                    self.release_holds(j, chosen, &mut holders);
                    progress = true;
                }
            }
            if (0..N).all(|j| !chosen[j] || resolved[j]) {
                return first_err;
            }
            if progress {
                continue;
            }

            let now_ms = (embassy_time::Instant::now() - epoch).as_millis();
            let mut deadline: Option<embassy_time::Instant> = None;
            for j in self.order_iter().rev() {
                if !chosen[j] || resolved[j] || signaled[j] == UNSIGNALED {
                    continue;
                }
                let node = self.nodes[j].expect("a chosen slot is occupied");
                let due_ms = signaled[j] as u64 + u64::from(node.cfg.ack_timeout_ms);
                if now_ms < due_ms {
                    let due = epoch + embassy_time::Duration::from_millis(due_ms);
                    deadline = Some(deadline.map_or(due, |d| d.min(due)));
                    continue;
                }
                warn!(
                    "supervisor: task {} did not ack shutdown within {}ms",
                    node.name(),
                    node.cfg.ack_timeout_ms,
                );
                #[cfg(feature = "budget")]
                if Self::has(shape::CLAIMS) {
                    node.release_claims();
                }
                note(
                    node,
                    Some(j),
                    Transition::Faulted(FaultKind::ShutdownTimeout),
                );
                let fault = NodeFault {
                    node,
                    kind: FaultKind::ShutdownTimeout,
                };
                if !keep_going {
                    return Err(fault);
                }
                if first_err.is_ok() {
                    first_err = Err(fault);
                }
                resolved[j] = true;
                self.release_holds(j, chosen, &mut holders);
                progress = true;
            }
            if progress {
                continue;
            }
            let deadline = deadline.expect("an unresolved wave has a signalled node");
            let _ = embassy_futures::select::select(STOP_EVT.wait_past(seen), Timer::at(deadline))
                .await;
        }
    }

    /// `j` resolved in a stop wave: its row no longer holds up its chosen deps.
    fn release_holds(&self, j: usize, chosen: &[bool; N], holders: &mut [u16; N]) {
        for &d in self.topo.deps_of(j as u8) {
            if chosen[d as usize] {
                holders[d as usize] -= 1;
            }
        }
    }

    async fn start_nodes(
        &self,
        spawner: &Spawner,
        select: &mut dyn FnMut(usize, &'static TaskNode) -> bool,
        keep_going: bool,
        policy: &mut dyn RunPolicy,
        origin: Origin,
    ) -> Result<(), NodeFault> {
        let mut pending = [false; N];
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if select(j, node) {
                pending[j] = true;
            }
        }

        // When each node's deps resolved, in ms since `epoch`: the start of its gate
        // budget, and with `slot_timeout` the deadline the wave parks until.
        // `UNARMED` until then.
        const UNARMED: u32 = u32::MAX;
        let epoch = embassy_time::Instant::now();
        let mut armed: [u32; N] = [UNARMED; N];
        let mut first_err = Ok(());
        // A `Busy` streak spans rounds without progress: its first round asks
        // for one executor pass, every later one backs off `BUSY_BACKOFF`.
        let mut busy_streak = false;
        loop {
            let seen = GATE_EVT.generation();
            let mut progress = false;
            let mut waiting = false;
            // A `Busy` spawn on the supervisor's own executor: a yield is the
            // pass that lets the previous instance's final poll return.
            let mut own_pass = false;
            // Some spawn attempt in this round came back `Busy`.
            let mut busy = false;
            'nodes: for j in self.order_iter() {
                if !pending[j] {
                    continue;
                }
                let node = self.nodes[j].expect("a pending slot is occupied");
                // Brought up meanwhile by another path, such as a pool driver
                // growing an `OnDemand` member this wave brings back: done.
                if node.is_running() {
                    pending[j] = false;
                    progress = true;
                    continue;
                }
                if self
                    .topo
                    .deps_of(j as u8)
                    .iter()
                    .any(|&d| pending[d as usize])
                {
                    waiting = true;
                    continue;
                }
                // A parked `Pause` instance is resumed in place: it kept its
                // resources, so only its `ready` deps are awaited. Read before any
                // reset clears the ack.
                let resumable = Self::has(shape::PAUSE)
                    && matches!(node.mode(), Mode::Pause)
                    && node.has_acked_stop();
                let budget_start = if armed[j] != UNARMED {
                    armed[j]
                } else {
                    let now = ceil_ms(embassy_time::Instant::now() - epoch).min(UNARMED as u64 - 1)
                        as u32;
                    armed[j] = now;
                    if !resumable {
                        note(node, Some(j), Transition::Spawning);
                        info!("supervisor: spawning {} ({})", node.name(), node.mode());
                    }
                    now
                };
                let spawn = if resumable {
                    None
                } else {
                    let Some(spawn) = node.cfg.spawn else {
                        // A parked node the app spawns itself; only marked.
                        node.arm();
                        node.mark_running();
                        note(node, Some(j), Transition::Started);
                        pending[j] = false;
                        progress = true;
                        continue;
                    };
                    Some(spawn)
                };
                // The gate sequence — executor slot, resources, ready deps —
                // tested without blocking; the first unsatisfied gate defers
                // the node to the next round, or faults it once its budget is
                // spent.
                let overdue = (embassy_time::Instant::now() - epoch).as_millis()
                    >= budget_start as u64 + u64::from(node.cfg.slot_timeout_ms);
                let mut unsatisfied = |kind: FaultKind| -> Result<bool, NodeFault> {
                    if !overdue {
                        return Ok(true);
                    }
                    let fault = NodeFault { node, kind };
                    note(node, Some(j), Transition::Faulted(kind));
                    if !keep_going {
                        return Err(fault);
                    }
                    warn!("supervisor: {}", fault);
                    // Every fault is reported, also after the first taken one,
                    // so a counting policy sees them all.
                    if policy.returns(&fault, origin) && first_err.is_ok() {
                        first_err = Err(fault);
                    }
                    Ok(false)
                };
                // A resumable instance skips the executor-slot and resource gates.
                let blocked = if resumable {
                    None
                } else if Self::has(shape::EXEC_SLOTS)
                    && node.cfg.spawn_slot.is_some_and(|s| s.get().is_none())
                {
                    Some(unsatisfied(FaultKind::ExecutorSlotEmpty)?)
                } else if Self::has(shape::RESOURCES) && !node.__sv_gates_filled() {
                    Some(unsatisfied(FaultKind::ResourceMissing)?)
                } else {
                    None
                };
                #[cfg(feature = "readiness")]
                let blocked = match blocked {
                    Some(b) => Some(b),
                    None if Self::has(shape::READY_DEPS) => {
                        match node.ready_deps().iter().find(|d| !d.is_ready()) {
                            Some(dep) => {
                                #[cfg(feature = "bound-deps")]
                                if overdue
                                    && node.bound_deps().iter().any(|b| core::ptr::eq(*b, *dep))
                                {
                                    info!(
                                        "supervisor: {} parked, bound dep {} not ready",
                                        node.name(),
                                        dep.name()
                                    );
                                    node.handle.flag_set(flag::BOUND_STOPPED);
                                    pending[j] = false;
                                    progress = true;
                                    continue 'nodes;
                                }
                                Some(unsatisfied(FaultKind::ReadyDepTimeout { dep })?)
                            }
                            None => None,
                        }
                    }
                    None => None,
                };
                // A resident shell still finishing its last activation. A
                // resumable instance is parked, not finishing: nothing to wait out.
                let blocked = match blocked {
                    None if !resumable && !node.is_settled() => {
                        Some(unsatisfied(FaultKind::Spawn(SpawnError::Busy))?)
                    }
                    b => b,
                };
                match blocked {
                    Some(true) => {
                        waiting = true;
                        continue 'nodes;
                    }
                    Some(false) => {
                        pending[j] = false;
                        progress = true;
                        continue 'nodes;
                    }
                    None => {}
                }
                let Some(spawn) = spawn else {
                    info!("supervisor: resuming {} in place", node.name());
                    node.resume_in_place();
                    note(node, Some(j), Transition::Resumed);
                    pending[j] = false;
                    progress = true;
                    continue;
                };
                // Reset immediately before the attempt, so nothing the
                // previous instance wrote late survives into this one. For a
                // `spawn:` node this holds when the task fn has no free pool
                // slot (`pool_size = 1`, the default): the spawn then fails
                // `Busy` until the previous instance has returned. With a
                // pooled task fn the spawn can succeed while the old instance
                // is still running, so such a body must make its ack
                // (`ack_dropped`, `mark_exited`, the `_acked` combinators) its
                // last use of the node: anything it does to the node afterwards
                // lands on the new activation.
                node.arm();
                if let Err(err) = spawn(*spawner) {
                    // A respawn can catch the previous instance still inside its
                    // final poll: the first `Busy` of a streak waits one pass of
                    // its executor, a later one backs off instead of spinning.
                    if unsatisfied(FaultKind::Spawn(err))? {
                        if !busy_streak {
                            match node.cfg.spawn_slot {
                                Some(slot) => {
                                    let _ = slot.request_pass();
                                }
                                None => own_pass = true,
                            }
                        }
                        busy = true;
                        waiting = true;
                    } else {
                        pending[j] = false;
                        progress = true;
                    }
                    continue;
                }
                node.mark_running();
                note(node, Some(j), Transition::Started);
                pending[j] = false;
                progress = true;
            }
            if !waiting {
                return first_err;
            }
            if progress {
                busy_streak = false;
                continue;
            }
            if own_pass {
                // First `Busy` of a streak on the supervisor's own executor: one
                // yield lets the previous instance's final poll return.
                busy_streak = true;
                embassy_futures::yield_now().await;
                continue;
            }
            let deadline_ms = self
                .order_iter()
                .filter(|&j| pending[j] && armed[j] != UNARMED)
                .map(|j| {
                    armed[j] as u64
                        + u64::from(
                            self.nodes[j]
                                .expect("a pending slot is occupied")
                                .cfg
                                .slot_timeout_ms,
                        )
                })
                .min()
                .expect("a waiting wave has an armed node");
            let mut deadline = epoch + embassy_time::Duration::from_millis(deadline_ms);
            if busy {
                if busy_streak {
                    deadline = deadline.min(embassy_time::Instant::now() + BUSY_BACKOFF);
                }
                busy_streak = true;
            }
            let _ = embassy_futures::select::select(GATE_EVT.wait_past(seen), Timer::at(deadline))
                .await;
        }
    }

    /// Stop a single node, waiting for its shutdown ack.
    pub async fn stop_node(&self, node: &'static TaskNode) -> Result<(), NodeFault> {
        if !node.is_running() || node.is_detached() {
            return Ok(());
        }
        self.shutdown_and_wait(node).await?;
        info!("supervisor: stopped {}", node.name());
        Ok(())
    }

    /// Stop every **running** node, dependents before their dependencies.
    /// Down `OnDemand` nodes are skipped (no instance to ack). Pause-mode nodes
    /// ack and park on `wait_resume()`; Terminate/OnDemand nodes exit.
    ///
    /// The stop runs as a wave: a node is signalled once every dependent
    /// stopping with it has acked, so a `deps:` dependency keeps serving
    /// through its dependents' cleanup, and a node with no such dependents is
    /// signalled at once. The first missed ack ends the wave with
    /// [`FaultKind::ShutdownTimeout`]; [`teardown_continue`](Self::teardown_continue)
    /// keeps going instead.
    pub async fn teardown(&self) -> Result<(), NodeFault> {
        self.stop_nodes(
            &mut |_, node| {
                if !node.is_running() || node.is_detached() {
                    return false;
                }
                info!("supervisor: tearing down {}", node.name());
                true
            },
            false,
        )
        .await
    }

    /// Like [`teardown`](Self::teardown), but best-effort: a node that misses
    /// its ack is faulted, its `divisible` claims are released, and the wave
    /// still stops its dependencies; the first [`FaultKind::ShutdownTimeout`]
    /// is returned once every chosen node has resolved.
    pub async fn teardown_continue(&self) -> Result<(), NodeFault> {
        self.stop_nodes(
            &mut |_, node| {
                if !node.is_running() || node.is_detached() {
                    return false;
                }
                info!("supervisor: tearing down {}", node.name());
                true
            },
            true,
        )
        .await
    }

    /// Resume a single parked Pause-mode node. A node stopped by a `bound`
    /// provider is skipped until that provider serves again.
    pub fn resume_node(&self, node: &'static TaskNode) {
        if !Self::has(shape::PAUSE)
            || !matches!(node.mode(), Mode::Pause)
            || node.is_disabled()
            || node.is_collateral()
            || node.is_detached()
            || !node.has_acked_stop()
            || !node.bound_ok()
        {
            return;
        }
        info!("supervisor: resuming {}", node.name());
        node.resume_in_place();
        self.note_verb(node, Transition::Resumed);
    }

    /// Signal every **parked** Pause-mode node to resume. Cheap and
    /// synchronous: the tasks were parked on `wait_resume()` and pick up
    /// immediately. Separate from `respawn_terminate` so the application can
    /// fire resume independently of the respawn step. Disabled (manually
    /// paused) nodes are skipped so a manual pause sticks, detached
    /// (self-managed) Pause nodes are left parked, and, as in
    /// [`resume_node`](Self::resume_node), a node without a parked instance
    /// (`has_acked_stop`) is skipped: signalling it would latch `resume_wake`
    /// with no waiter, and the node's *next* park would end at once. A node
    /// stopped by a `bound` provider is skipped until that provider serves
    /// again.
    pub fn resume_pausable(&self) {
        if !Self::has(shape::PAUSE) {
            return;
        }
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if matches!(node.mode(), Mode::Pause)
                && !node.is_disabled()
                && !node.is_collateral()
                && !node.is_detached()
                && node.has_acked_stop()
                && node.bound_ok()
            {
                info!("supervisor: resuming {}", node.name());
                node.resume_in_place();
                note(node, Some(j), Transition::Resumed);
            }
        }
    }

    /// Restart every Terminate-mode node that is down and not disabled,
    /// collateral or detached, as one start wave through the full gate
    /// sequence. The wave stops at the first fault and returns it; nodes not
    /// yet started stay down.
    pub async fn respawn_terminate(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        self.start_nodes(
            spawner,
            &mut |_, node| {
                matches!(node.mode(), Mode::Terminate)
                    && !node.is_disabled()
                    && !node.is_collateral()
                    && !node.is_detached()
                    && !node.is_running()
            },
            false,
            &mut KeepFirst,
            Origin::Boot,
        )
        .await
    }
}

#[cfg(any(feature = "control", feature = "pool"))]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Whether every dependency of `node` is currently running, resolved through
    /// the graph's index table. The pool driver checks this before growing a
    /// member, so a grow never spawns a node whose deps are down.
    #[cfg(feature = "pool")]
    pub(crate) fn deps_running(&self, node: &'static TaskNode) -> bool {
        match self.index_of(node) {
            Some(i) => self
                .topo
                .deps_of(i as u8)
                .iter()
                .all(|&di| self.nodes[di as usize].is_none_or(|n| n.is_running())),
            None => false,
        }
    }
}

#[cfg(feature = "control")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Seed a membership set with `target` plus — if `target` belongs to an
    /// elastic pool — every member of that pool, so control is applied to the
    /// whole pool atomically. Pool membership is read from `GRAPH.pools`; with no
    /// pools (the `pool` feature off, or none declared) this is just `{target}`.
    fn seed(&self, target: &'static TaskNode, set: &mut [bool; N]) {
        if let Some(i) = self.index_of(target) {
            set[i] = true;
        }
        #[cfg(feature = "pool")]
        if Self::has(shape::POOLS) {
            for pool in self.pools {
                let members = pool.members();
                if members.iter().any(|m| core::ptr::eq(*m, target)) {
                    for m in members {
                        if let Some(i) = self.index_of(m) {
                            set[i] = true;
                        }
                    }
                }
            }
        }
    }

    fn collect_dependents(&self, set: &mut [bool; N]) {
        for j in self.order_iter() {
            if set[j] {
                continue;
            }
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if node.is_detached() {
                continue;
            }
            if self
                .topo
                .deps_of(j as u8)
                .iter()
                .any(|&di| set[di as usize])
            {
                set[j] = true;
            }
        }
    }

    /// Apply a control command to the requested node.
    ///
    /// Activate propagates the first startup fault after completing its best-effort
    /// wave when called directly (here or through `activate`); the driver in
    /// [`run`](Self::run) logs and absorbs an `Activate` bring-up fault and
    /// keeps serving. Restart propagates shutdown timeouts (and, with
    /// `restart-intensity`, a refused [`RestartStorm`](FaultKind::RestartStorm))
    /// but logs and absorbs its startup faults.
    pub async fn apply_control(
        &self,
        cmd: ControlCommand,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        match cmd.op {
            ControlOp::Deactivate => self.deactivate(cmd.node).await,
            ControlOp::Activate => self.activate(cmd.node, spawner).await,
            #[cfg(feature = "restart")]
            ControlOp::Restart => match self.restart(cmd.node, spawner).await {
                Ok(()) => Ok(()),
                Err(e) if matches!(e.kind, FaultKind::ShutdownTimeout) => Err(e),
                #[cfg(feature = "restart-intensity")]
                Err(e) if matches!(e.kind, FaultKind::RestartStorm) => Err(e),
                Err(e) => {
                    let node = e.node;
                    warn!(
                        "supervisor: {} did not come back after restart",
                        node.name()
                    );
                    Ok(())
                }
            },
        }
    }

    /// Deactivate the target node and all of its dependents.
    pub async fn deactivate(&self, target: &'static TaskNode) -> Result<(), NodeFault> {
        self.deactivate_with(target, &mut KeepFirst).await
    }

    /// [`deactivate`](Self::deactivate), with a missed ack sent to `policy`.
    async fn deactivate_with(
        &self,
        target: &'static TaskNode,
        policy: &mut dyn RunPolicy,
    ) -> Result<(), NodeFault> {
        let mut seed = [false; N];
        self.seed(target, &mut seed);
        let mut set = seed;
        self.collect_dependents(&mut set);

        // Down in reverse topo order (dependents before their deps).
        self.stop_nodes(
            &mut |j, node| {
                if !set[j] {
                    return false;
                }
                // A detached node is self-managed — never control-stop it. The growth
                // loop keeps detached *dependents* out of the set; this also covers a
                // detached node that was seeded directly (or a detached pool member).
                // Without it a detached one-shot that already exited (stale
                // `is_running`, no ack path) would be signalled a shutdown it can never
                // acknowledge, failing here with a spurious missed-ack fault.
                if node.is_detached() {
                    return false;
                }
                if seed[j] {
                    node.set_disabled(true);
                } else {
                    node.handle.flag_set(flag::COLLATERAL);
                }
                if !node.is_running() {
                    return false;
                }
                info!("supervisor: control-stop {}", node.name());
                true
            },
            false,
        )
        .await
        .or_else(|f| taken(policy, f, Origin::Deactivate))
    }

    /// Bring `target`, its pool and every transitive dependency up in topological
    /// order, clearing their disabled latches and reviving eligible collateral
    /// dependents. Running and detached nodes are skipped; parked Pause nodes
    /// resume in place. OnDemand nodes remain deferred to pool demand.
    ///
    /// Completes the best-effort startup wave and returns its first [`NodeFault`],
    /// or `Ok(())` if no eligible work fails (including when no work is selected).
    /// A failure does not roll back successful starts: the graph may be partially
    /// running. Pool scaling is requested even when startup returns a fault.
    pub async fn activate(
        &self,
        target: &'static TaskNode,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        self.activate_with(target, spawner, &mut KeepFirst).await
    }

    /// [`activate`](Self::activate), reporting each fault of its wave and
    /// returning the first one `policy` takes.
    async fn activate_with(
        &self,
        target: &'static TaskNode,
        spawner: &Spawner,
        policy: &mut dyn RunPolicy,
    ) -> Result<(), NodeFault> {
        let mut set = [false; N];
        self.seed(target, &mut set);

        // Grow the set to include transitive deps. Walk dependents-first
        // (reverse topo); when a set member is seen, pull in its direct deps.
        // A detached member's `deps:` are start-ordering only (the node is
        // self-managed and nothing here starts it), so they are not pulled in.
        for j in self.order_iter().rev() {
            if set[j] && !self.nodes[j].is_some_and(|n| n.is_detached()) {
                for &di in self.topo.deps_of(j as u8) {
                    set[di as usize] = true;
                }
            }
        }
        for j in self.order_iter() {
            if set[j]
                && let Some(node) = self.nodes[j]
                && !node.is_detached()
            {
                node.set_disabled(false);
            }
        }
        let mut held = [false; N]; // disabled, or depends on a disabled node
        let mut revive = [false; N];
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if node.is_detached() {
                continue;
            }
            let blocked = self
                .topo
                .deps_of(j as u8)
                .iter()
                .any(|&di| held[di as usize]);
            held[j] = blocked || node.is_disabled();
            if !blocked && node.is_collateral() {
                node.handle.flag_clear(flag::COLLATERAL);
                revive[j] = !node.is_disabled();
            }
        }

        let r = self
            .start_nodes(
                spawner,
                &mut |j, node| {
                    if !(set[j] || revive[j]) || node.is_detached() {
                        return false;
                    }
                    !node.is_running()
                        && !node.is_collateral()
                        && !(Self::has(shape::ON_DEMAND) && matches!(node.mode(), Mode::OnDemand))
                },
                true,
                policy,
                Origin::Activate,
            )
            .await;
        #[cfg(feature = "pool")]
        if Self::has(shape::POOLS) {
            request_scale();
        }
        r
    }
}

#[cfg(feature = "bound-deps")]
pub(crate) fn is_serving(node: &TaskNode) -> bool {
    node.is_running() && node.is_ready()
}

#[cfg(feature = "bound-deps")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// React to bound-dependency readiness changes by stopping or resuming nodes.
    ///
    /// Returns a missed ack of the stop half. A node that cannot be brought
    /// back is logged and left down.
    pub async fn apply_bind(&self, spawner: &Spawner) -> Result<(), NodeFault> {
        self.apply_bind_with(spawner, &mut ReturnOnHalt).await
    }

    /// [`apply_bind`](Self::apply_bind), with each fault sent to `policy`.
    pub(crate) async fn apply_bind_with(
        &self,
        spawner: &Spawner,
        policy: &mut dyn RunPolicy,
    ) -> Result<(), NodeFault> {
        if !Self::has(shape::BOUND_DEPS) {
            return Ok(());
        }
        let mut stopping = [false; N];
        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if node.is_detached() || !node.is_running() {
                continue;
            }
            let mut down = false;
            for d in node.bound_deps() {
                if !is_serving(d) {
                    down = true;
                    break;
                }
                for (k, slot) in self.nodes.iter().enumerate() {
                    if stopping[k] && slot.is_some_and(|x| core::ptr::eq(x, *d)) {
                        down = true;
                        break;
                    }
                }
                if down {
                    break;
                }
            }
            stopping[j] = down;
        }

        if let Err(f) = self
            .stop_nodes(
                &mut |j, node| {
                    if !stopping[j] {
                        return false;
                    }
                    info!(
                        "supervisor: bound-stop {} (a bound provider withdrew readiness)",
                        node.name()
                    );
                    node.handle.flag_set(flag::BOUND_STOPPED);
                    true
                },
                false,
            )
            .await
        {
            return taken(policy, f, Origin::Bind);
        }

        for j in self.order_iter() {
            let Some(node) = self.nodes[j] else {
                continue;
            };
            if !node.handle.flag(flag::BOUND_STOPPED) {
                continue;
            }
            if node.is_disabled() || node.is_collateral() || node.is_detached() || node.is_running()
            {
                continue;
            }
            if !node.bound_deps().iter().all(|d| is_serving(d)) {
                continue;
            }
            match node.mode() {
                // A parked instance resumes in place, keeping its resources.
                Mode::Pause if Self::has(shape::PAUSE) && node.has_acked_stop() => {
                    info!("supervisor: bound-resume {}", node.name());
                    node.resume_in_place();
                    note(node, Some(j), Transition::Resumed);
                }
                // Anything else, including a Pause node parked at bring-up
                // before it ever ran, starts through the full gate sequence.
                Mode::Terminate | Mode::Pause => {
                    info!("supervisor: bound-restart {}", node.name());
                    if let Err(f) = self.start_node(node, spawner).await {
                        if policy.returns(&f, Origin::Bind) {
                            return Err(f);
                        }
                        warn!("supervisor: {} could not be bound-restarted", node.name());
                    }
                }
                Mode::OnDemand => {
                    node.handle.flag_clear(flag::BOUND_STOPPED);
                    #[cfg(feature = "pool")]
                    if Self::has(shape::POOLS) {
                        request_scale();
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(feature = "restart")]
impl<const N: usize, T: Topology<N>> Supervisor<N, T> {
    /// Restart the target node and all of its dependents.
    ///
    /// What was running when the call began comes back, whatever its latch or
    /// mode: a restart is a cycle, so a `disabled` node started by hand and a
    /// running `OnDemand` node are not left down. A member that was down comes
    /// up only when it is enabled, not collateral and not `OnDemand`. The way
    /// back up is a best-effort wave: every member that can come back does, and
    /// the first fault is returned once the wave has settled.
    ///
    /// With `restart-intensity`, a target carrying `restart_budget: MAX in
    /// WINDOW_MS` is checked first: the MAX+1th restart inside the window
    /// returns [`FaultKind::RestartStorm`] before anything is stopped. Only
    /// this verb's *target* is counted: dependents cycled with it, and
    /// `activate`, never are. The window opens at the first counted restart
    /// and a restart after it closes opens a fresh one. A target outside this
    /// graph is a no-op and is not counted.
    pub async fn restart(
        &self,
        target: &'static TaskNode,
        spawner: &Spawner,
    ) -> Result<(), NodeFault> {
        self.restart_with(target, spawner, &mut KeepFirst).await
    }

    /// [`restart`](Self::restart), reporting each fault: a refused restart, a
    /// missed ack of the stop half, every fault of the way back up.
    async fn restart_with(
        &self,
        target: &'static TaskNode,
        spawner: &Spawner,
        policy: &mut dyn RunPolicy,
    ) -> Result<(), NodeFault> {
        // Outside this graph: nothing to count or cycle, so one driver can
        // apply a Restart to every supervisor in turn.
        if self.index_of(target).is_none() {
            return Ok(());
        }
        #[cfg(feature = "restart-intensity")]
        if let Some((max, window)) = target.restart_budget() {
            // Only the supervisor task calls `restart`: the count and the
            // window start have a single writer, so plain loads and stores.
            let h = &target.handle;
            let now = embassy_time::Instant::now().as_ticks();
            let start = (u64::from(h.restart_window_hi.load(Ordering::Relaxed)) << 32)
                | u64::from(h.restart_window_lo.load(Ordering::Relaxed));
            let count = h.restarts.load(Ordering::Relaxed);
            if count == 0 || now.saturating_sub(start) > window.as_ticks() {
                h.restart_window_lo.store(now as u32, Ordering::Relaxed);
                h.restart_window_hi
                    .store((now >> 32) as u32, Ordering::Relaxed);
                h.restarts.store(1, Ordering::Relaxed);
            } else if count >= max {
                warn!(
                    "supervisor: {} restart budget exhausted ({} in window)",
                    target.name(),
                    max
                );
                #[cfg(any(feature = "lifecycle-events", feature = "lifecycle-sink"))]
                emit_lifecycle(LifecycleEvent {
                    node: target,
                    kind: LifecycleKind::Faulted(FaultKind::RestartStorm),
                });
                let fault = NodeFault {
                    node: target,
                    kind: FaultKind::RestartStorm,
                };
                return taken(policy, fault, Origin::Restart);
            } else {
                h.restarts.store(count + 1, Ordering::Relaxed);
            }
        }

        let mut set = [false; N];
        self.seed(target, &mut set);
        self.collect_dependents(&mut set);

        // What was running comes back whatever its latch or mode: a restart is a
        // cycle. A `disabled` node started by hand, or a running `OnDemand` node,
        // is not left down.
        let was_running: [bool; N] = core::array::from_fn(|j| {
            set[j] && self.nodes[j].is_some_and(|n| !n.is_detached() && n.is_running())
        });

        info!(
            "supervisor: restarting {} and its dependents",
            target.name()
        );

        // Down, dependents first. No `disabled` latch: this is a cycle, not a
        // stop, and a concurrent observer must never see the subtree as
        // deliberately disabled.
        if let Err(f) = self
            .stop_nodes(
                &mut |j, node| {
                    if !set[j] || node.is_detached() || !node.is_running() {
                        return false;
                    }
                    info!("supervisor: stopping {}", node.name());
                    true
                },
                false,
            )
            .await
        {
            return taken(policy, f, Origin::Restart);
        }

        // Up, dependencies first, each through the full gate sequence, as a
        // best-effort wave: every member that can come back does, and the first
        // fault is returned.
        let r = self
            .start_nodes(
                spawner,
                &mut |j, node| {
                    set[j]
                        && !node.is_detached()
                        && !node.is_running()
                        && (was_running[j]
                            || (!node.is_disabled()
                                && !node.is_collateral()
                                && !(Self::has(shape::ON_DEMAND)
                                    && matches!(node.mode(), Mode::OnDemand))))
                },
                true,
                policy,
                Origin::Restart,
            )
            .await;
        #[cfg(feature = "pool")]
        if Self::has(shape::POOLS) {
            request_scale();
        }

        r
    }
}

/// Where `Supervisor::run_with` met a fault, as its [`RunPolicy`] sees it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Origin {
    /// The boot wave at the start of `run_with`.
    Boot,
    /// A control `Activate` (feature `control`).
    Activate,
    /// A control `Deactivate` (feature `control`).
    Deactivate,
    /// A control `Restart` (feature `restart`): the refused restart, the stop
    /// half or the way back up.
    Restart,
    /// A bind cascade after a bound provider changed readiness (feature
    /// `bound-deps`).
    Bind,
    /// A pool pass growing a member (feature `pool`).
    PoolGrow,
    /// A pool pass stopping a member (feature `pool`).
    PoolShrink,
}

/// The policy of the public verbs: every fault is taken, so each verb
/// returns its first.
struct KeepFirst;

impl RunPolicy for KeepFirst {
    fn returns(&mut self, _: &NodeFault, _: Origin) -> bool {
        true
    }
}

/// A fault that ends its step: `Err` when `policy` takes it, `Ok` when it is
/// absorbed.
#[cfg(feature = "control")]
fn taken(policy: &mut dyn RunPolicy, fault: NodeFault, origin: Origin) -> Result<(), NodeFault> {
    if policy.returns(&fault, origin) {
        Err(fault)
    } else {
        Ok(())
    }
}

/// Which faults end `Supervisor::run_with`.
///
/// `returns` is called once for every fault the driver meets, in order, with
/// where it happened. `true` ends `run_with` with that fault. `false` keeps the
/// driver going: the fault is still logged and reported as `Faulted`.
/// [`FaultKind::is_bring_up`] separates the faults that leave a node down
/// from the ones about a node that is still running.
///
/// The policy is a value, so it can count or carry settings. It runs inside
/// the driver's waves and must only observe: to act on a fault, send a
/// command with `try_request_control`, which the driver applies afterwards.
///
/// ```
/// use embassy_supervisor::{FaultKind, NodeFault, Origin, RunPolicy};
///
/// /// Return a halt at once, and a bring-up fault on its third occurrence.
/// struct ThreeStrikes {
///     bring_up_faults: u8,
/// }
/// impl RunPolicy for ThreeStrikes {
///     fn returns(&mut self, fault: &NodeFault, _origin: Origin) -> bool {
///         if !fault.kind.is_bring_up() {
///             return true;
///         }
///         self.bring_up_faults += 1;
///         self.bring_up_faults >= 3
///     }
/// }
/// ```
pub trait RunPolicy {
    /// True when `run_with` returns this fault.
    fn returns(&mut self, fault: &NodeFault, origin: Origin) -> bool;
}

/// The policy of `Supervisor::run`: return only when a node halts, that is
/// a missed shutdown ack (`ShutdownTimeout`) or a refused restart
/// (`RestartStorm`). Bring-up faults are logged and absorbed.
pub struct ReturnOnHalt;

impl RunPolicy for ReturnOnHalt {
    fn returns(&mut self, fault: &NodeFault, _: Origin) -> bool {
        !fault.kind.is_bring_up()
    }
}

/// Return on every fault, bring-up faults included.
pub struct FailFast;

impl RunPolicy for FailFast {
    fn returns(&mut self, _: &NodeFault, _: Origin) -> bool {
        true
    }
}
