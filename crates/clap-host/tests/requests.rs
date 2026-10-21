//! What a hosted CLAP plugin asks of its host is answered on the main thread, by the plugin's
//! service handle (`Plugin::service_handle`), with the editor open or not. The fixture plugin
//! `org.soundcraft.test.requests` makes each request on a trigger the test controls and is strict:
//! it aborts the process when the host activates, deactivates or calls it back on the wrong thread,
//! or restarts it while `process` runs on another thread. Every answer is read from outside the
//! host: through the plugin's own live parameter values, its editor and its audio.

mod common;

use common::fixture_dir;
use soundcraft_dsp::Plugin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const REQUESTS: &str = "clap:org.soundcraft.test.requests";
const SYNTH: &str = "clap:org.soundcraft.test.synth";

fn requests() -> Box<dyn Plugin> {
    fixture_dir();
    soundcraft_clap_host::create(REQUESTS).expect("the requests fixture")
}

/// Tells the fixture to make request `kind` (see its module docs), from the calling thread.
fn trigger(p: &mut Box<dyn Plugin>, kind: u8) {
    assert!(p.load_state(&[b'R', b'E', b'Q', kind]), "the fixture takes its trigger");
}

#[test]
// Contract: `request_callback` is answered with `on_main_thread` on the main thread for every
// hosted plugin, whether or not its editor is open (the fixture aborts on any other thread).
fn request_callback_runs_on_main_thread_without_an_editor() {
    let mut p = requests();
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(svc.live_param("21"), Some(0.0));
    trigger(&mut p, 2);
    assert_eq!(svc.live_param("21"), Some(0.0), "nothing runs before the service ticks");
    svc.service();
    assert_eq!(svc.live_param("21"), Some(1.0), "on_main_thread ran once");
    svc.service();
    assert_eq!(svc.live_param("21"), Some(1.0), "a request is answered once");
}

#[test]
// Contract: `state.mark_dirty` reaches the session: the service reports the plugin's state changed.
fn mark_dirty_is_reported_once() {
    let mut p = requests();
    let mut svc = p.service_handle().expect("service handle");
    assert!(!svc.service().dirty);
    trigger(&mut p, 4);
    assert!(svc.service().dirty, "the plugin's state changed");
    assert!(!svc.service().dirty, "reported once");
}

#[test]
// Contract: `params.rescan` (a program change that moved the plugin's values) makes the host
// re-read: the report says the values changed, and the live value is the plugin's own.
fn params_rescan_reports_changed_values() {
    let mut p = requests();
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(svc.live_param("7"), Some(1.0));
    trigger(&mut p, 3);
    let r = svc.service();
    assert!(r.values_changed, "the rescan was noticed");
    assert_eq!(svc.live_param("7"), Some(1.5), "the shown value comes from the live plugin");
    assert!(!svc.service().values_changed, "reported once");
}

#[test]
// Contract: `params.request_flush` while the plugin is not processing is answered with a
// `params.flush`, so the output events the plugin has for the host (its GUI edits) are not lost.
fn request_flush_without_processing_delivers_the_plugins_output_events() {
    let mut p = requests();
    let mut ed = p.editor().expect("editor handle");
    let mut svc = p.service_handle().expect("service handle");
    trigger(&mut p, 5);
    assert!(ed.idle().is_empty());
    svc.service();
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)], "the flushed edit reaches the editor's idle");
}

#[test]
// Contract: a restart (`request_restart`) deactivates and re-activates the plugin on the main
// thread while the audio thread keeps running without ever locking: blocks while it is parked
// pass the input through, afterwards the plugin processes again with its refreshed cookies. The
// fixture aborts when `activate`/`deactivate` run off the main thread or during a `process`.
fn restart_reactivates_on_the_main_thread_while_audio_runs() {
    let mut p = requests();
    let mut svc = p.service_handle().expect("service handle");
    assert!(p.set_param("7", 2.0));
    assert_eq!(svc.live_param("22"), Some(1.0), "activated once by prepare");
    let (tx, rx) = mpsc::channel::<(&'static str, f32)>();
    let stop = Arc::new(AtomicBool::new(false));
    let last = Arc::new(AtomicU32::new(0));
    let audio = {
        let (stop, last) = (Arc::clone(&stop), Arc::clone(&last));
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                while let Ok((id, v)) = rx.try_recv() {
                    p.set_param(id, v);
                }
                let mut io = vec![vec![0.5f32; 64]; 2];
                p.process(&mut io, 64);
                last.store(io[0][10].to_bits(), Ordering::Relaxed);
                seen.push(io[0][10]);
                std::thread::sleep(Duration::from_micros(200));
            }
            (p, seen)
        })
    };
    let wait = |what: &str, mut done: Box<dyn FnMut() -> bool>| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let heard = |v: f32, last: &Arc<AtomicU32>| (f32::from_bits(last.load(Ordering::Relaxed)) - v).abs() < 1e-6;
    wait(
        "the plugin to process",
        Box::new({
            let last = Arc::clone(&last);
            move || heard(1.0, &last)
        }),
    );
    tx.send(("20", 1.0)).expect("audio thread");
    wait(
        "the restart",
        Box::new({
            move || {
                svc.service();
                svc.live_param("22") == Some(2.0)
            }
        }),
    );
    // The plugin changed its cookies while deactivated: a parameter change must still reach it.
    tx.send(("7", 0.5)).expect("audio thread");
    wait(
        "a parameter change to reach the restarted plugin",
        Box::new({
            let last = Arc::clone(&last);
            move || heard(0.25, &last)
        }),
    );
    stop.store(true, Ordering::Relaxed);
    let (p, seen) = audio.join().expect("audio thread");
    assert!(!p.failed(), "a restart is not a failure");
    assert_eq!(p.latency(), 3, "latency read again after the restart");
    assert!(
        seen.iter().all(|&v| [0.5, 1.0, 0.25].iter().any(|w| (v - w).abs() < 1e-6)),
        "only the plugin's sound or the passed-through input is heard: {seen:?}"
    );
}

#[test]
// Contract: a parameter's shown value comes from the live plugin, not from its advertised default
// (the synth starts at 1.0 and advertises 0.0, like Surge XT's ~800 parameters).
fn live_param_is_the_plugins_value_not_its_advertised_default() {
    fixture_dir();
    let info = soundcraft_clap_host::plugin_info(SYNTH).expect("info");
    assert_eq!(info.param("7").map(|p| p.default), Some(0.0), "advertised");
    let mut p = soundcraft_clap_host::create(SYNTH).expect("synth");
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(svc.live_param("7"), Some(1.0), "live");
    assert_eq!(svc.live_param("no-such"), None);
}

#[test]
// Contract: `clap.thread-check` tells a plugin the truth: on the creating thread it is the main
// thread and not the audio thread; inside `process` (here on another thread) it is the audio
// thread and not the main thread.
fn thread_check_tells_main_from_audio() {
    let mut p = requests();
    let mut svc = p.service_handle().expect("service handle");
    trigger(&mut p, 10);
    assert_eq!(svc.live_param("23"), Some(11.0), "on the main thread: main = true, audio = false");
    assert_eq!(svc.live_param("24"), Some(0.0), "not asked in process yet");
    let p = std::thread::spawn(move || {
        let mut io = vec![vec![0.5f32; 64]; 2];
        p.process(&mut io, 64);
        p
    })
    .join()
    .expect("audio thread");
    assert_eq!(svc.live_param("24"), Some(12.0), "in process: main = false, audio = true");
    drop(p);
}
