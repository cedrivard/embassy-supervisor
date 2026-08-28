//! `init_host_logging`: the one-call stderr sink for hosted targets. The
//! line format itself lands on stderr (not asserted — the harness owns the
//! streams); what this locks is the install contract.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node SINK = Terminate, deps: [];
}

#[test]
fn installs_once_and_logs_through() {
    embassy_supervisor::init_host_logging(log::LevelFilter::Trace).expect("first install");
    assert_eq!(log::max_level(), log::LevelFilter::Trace);

    // The sink is live: a status report routes through the `log` backend and
    // this logger without panicking.
    SINK.report_status("degraded: no input");

    // The global logger slot is single-occupancy; a second call reports it
    // and leaves the installed logger and level untouched.
    assert!(embassy_supervisor::init_host_logging(log::LevelFilter::Error).is_err());
    assert_eq!(log::max_level(), log::LevelFilter::Trace);
}
