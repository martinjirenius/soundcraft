//! A process failure is logged with its reason within one service tick, not at the next `prepare`
//! (which may be far off). Its own test binary: the log capture is process-wide.

mod common;

use std::sync::Mutex;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        if let Ok(mut l) = LOG.lock() {
            l.push(format!("{}", r.args()));
        }
    }
    fn flush(&self) {}
}

#[test]
// Contract: every plugin failure is reported with a reason, promptly (within one service tick).
fn a_process_failure_is_logged_with_its_reason_at_the_next_service_tick() {
    let found = soundcraft_vst3_host::scan_paths(&[common::fixture_dir().to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == "Fixture Gain").expect("the gain fixture").id;
    log::set_logger(&Capture).expect("logger");
    log::set_max_level(log::LevelFilter::Warn);
    let mut p = soundcraft_vst3_host::create(&id).expect("instantiate");
    let mut svc = p.service_handle().expect("service handle");
    // A gain of exactly 0 makes the fixture's `process` return kResultFalse.
    assert!(p.set_param("7", 0.0));
    let mut io = vec![vec![0.5f32; 64]; 2];
    p.process(&mut io, 64);
    assert!(p.failed());
    assert!(!LOG.lock().expect("log").iter().any(|l| l.contains("processing failed")), "nothing is logged on the audio thread");
    svc.service();
    let log = LOG.lock().expect("log").clone();
    assert!(log.iter().any(|l| l.contains("VST3 processing failed: process returned")), "the reason is logged by the service tick: {log:?}");
}
