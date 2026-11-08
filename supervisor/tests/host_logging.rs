//! `init_host_logging`: the one-call stderr sink for hosted targets. The
//! line format itself lands on stderr (not asserted — the harness owns the
//! streams); what this locks is the install contract.

#[test]
fn installs_once_and_logs_through() {
    embassy_supervisor::init_host_logging(log::LevelFilter::Trace).expect("first install");
    assert_eq!(log::max_level(), log::LevelFilter::Trace);

    // The sink is live: the facade routes records to it, not to the no-op default.
    assert!(
        log::logger().enabled(&log::Metadata::builder().level(log::Level::Info).build()),
        "the installed sink takes the records"
    );

    // The global logger slot is single-occupancy; a second call reports it
    // and leaves the installed logger and level untouched.
    assert!(embassy_supervisor::init_host_logging(log::LevelFilter::Error).is_err());
    assert_eq!(log::max_level(), log::LevelFilter::Trace);
}
