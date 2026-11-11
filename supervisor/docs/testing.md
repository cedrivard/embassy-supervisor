# Testing on the host

The crate is HAL-free, so graphs run on a desktop for tests: embassy-executor's
`platform-std` + `executor-thread` features give a std `Executor` to run on a thread,
and `embassy-time`'s `mock-driver` provides the clock (also enable
`critical-section/std`). The whole harness is ~15 lines:

```rust,ignore
#[embassy_executor::task]
async fn driver(spawner: embassy_executor::Spawner) {
    let sup = Supervisor::new(&GRAPH);
    sup.start(&spawner).await.expect("bring-up");
    // ... assertions, teardown/start cycles ...
    DONE.store(true, Ordering::Release);
}

fn main() {
    let clock = embassy_time::MockDriver::get();
    std::thread::spawn(|| {
        let ex: &'static mut embassy_executor::Executor =
            Box::leak(Box::new(embassy_executor::Executor::new()));
        ex.run(|spawner| spawner.spawn(driver(spawner).unwrap()));
    });
    while !DONE.load(Ordering::Acquire) {
        // Advance ONLY to observe a timeout (a shutdown-ack or gate
        // NodeFault) or liveness staleness; cross-thread advance is sound.
        // clock.advance(embassy_time::Duration::from_millis(500));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
```

A frozen mock clock is fine on the happy paths: every wait resolves by signal (acks,
slot fills, readiness), and the internal timeouts exist only to convert a *failure*
into an error, so advance the clock only when a test wants to observe
a `NodeFault` (a missed ack, an empty gate), or `is_stale` flipping (the liveness clock IS
embassy-time, so the mock drives it too). `heap-state` needs no `#[global_allocator]`
on the host; std provides one. The crate's own integration tests are all built this
way.
