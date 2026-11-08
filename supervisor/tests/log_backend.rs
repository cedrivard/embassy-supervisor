// `defmt` being on changes nothing here: the defmt backend is embedded-only
// (target_os = "none"), so on this host target `log` is the live backend
// even under --all-features.

use embassy_supervisor::supervisor_graph;
use std::sync::Mutex;

pub static PRODUCED: u32 = 0;
/// One static, read twice: the same pointer, so the second report is no change.
static DEGRADED: &str = "degraded: no input";

supervisor_graph! {
    node SOURCE = Terminate, deps: [], writes: [crate::PRODUCED];
    node SINK = Terminate, deps: [SOURCE], reads: [crate::PRODUCED];
}

struct Capture;

static RECORDS: Mutex<Vec<(log::Level, String)>> = Mutex::new(Vec::new());

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        RECORDS
            .lock()
            .unwrap()
            .push((record.level(), record.args().to_string()));
    }
    fn flush(&self) {}
}

#[test]
fn node_reports_reach_the_log_facade() {
    log::set_logger(&Capture).expect("no other logger is installed in this test binary");
    log::set_max_level(log::LevelFilter::Trace);

    SINK.report_status(DEGRADED);
    SINK.report_status(DEGRADED);

    let records = RECORDS.lock().unwrap();
    assert_eq!(records.len(), 1, "one change, one record: {records:?}");

    let (level, message) = &records[0];
    assert_eq!(*level, log::Level::Info);
    assert!(
        message.contains("sink") && message.contains(DEGRADED),
        "message names the node and its status: {message}"
    );
}
