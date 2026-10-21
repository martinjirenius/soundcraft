//! What a hosted VST3 plugin asks of its host is answered on the main thread, by the plugin's
//! service handle (`Plugin::service_handle`). The fixture's gain plugin makes each request on a
//! trigger the test controls (see the fixture's module docs) and is strict: it aborts the process
//! when the host calls `setActive`, `getLatencySamples` or `setupProcessing` off the thread that
//! created it, or re-activates it while `process` runs on another thread. Every answer is read
//! from outside the host: through the plugin's live values, its latency and its audio.

mod common;

use soundcraft_dsp::Plugin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn gain_id() -> String {
    let found = soundcraft_vst3_host::scan_paths(&[common::fixture_dir().to_path_buf()]);
    found.into_iter().find(|d| d.name == "Fixture Gain").expect("the gain fixture").id
}

fn gain() -> Box<dyn Plugin> {
    soundcraft_vst3_host::create(&gain_id()).expect("instantiate")
}

#[test]
// Contract: the host provides `IMessage`/`IAttributeList` through `IHostApplication::createInstance`
// (plugins exchange messages between component and controller): the component's message, with
// an int, a float, a string and a binary, arrives intact at the controller (which moves "Freq").
fn the_host_creates_messages_for_the_plugin() {
    let mut p = gain();
    let mut svc = p.service_handle().expect("service handle");
    let before = svc.live_param("12").expect("Freq");
    assert!((before - 632.46).abs() < 0.1, "{before}");
    assert!(p.load_state(b"MSG"), "the plugin could not allocate a message through the host");
    let after = svc.live_param("12").expect("Freq");
    assert!((after - 112.47).abs() < 0.1, "the message arrived intact: {after}");
}

#[test]
// Contract: `restartComponent(kParamValuesChanged)` and `setDirty` reach the host: the service
// reports the values changed and the state changed, and the live value is the plugin's own.
fn a_program_change_and_set_dirty_are_reported() {
    let mut p = gain();
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(svc.live_param("7"), Some(0.5));
    assert!(p.load_state(&[b'R', b'E', b'Q', 2]), "the fixture takes its trigger");
    let r = svc.service();
    assert!(r.values_changed, "kParamValuesChanged was noticed");
    assert!(r.dirty, "setDirty was noticed");
    assert_eq!(svc.live_param("7"), Some(1.8), "the shown value comes from the live plugin");
    let r = svc.service();
    assert!(!r.values_changed && !r.dirty, "reported once");
}

#[test]
// Contract: `restartComponent(kLatencyChanged)` is answered with a deactivate and re-activate on
// the main thread while the audio thread keeps running without locking (blocks while the plugin
// is parked pass the input through), and the new latency is read after `setActive(true)`, on the
// main thread, never from the audio thread (the fixture aborts otherwise).
fn a_latency_change_restarts_the_plugin_on_the_main_thread_while_audio_runs() {
    let mut p = gain();
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(p.latency(), 3);
    assert!(p.set_param("7", 2.0));
    let (tx, rx) = mpsc::channel::<(&'static str, f32)>();
    let stop = Arc::new(AtomicBool::new(false));
    let latency = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(AtomicU32::new(0));
    let audio = {
        let (stop, latency, last) = (Arc::clone(&stop), Arc::clone(&latency), Arc::clone(&last));
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                while let Ok((id, v)) = rx.try_recv() {
                    p.set_param(id, v);
                }
                let mut io = vec![vec![0.5f32; 64]; 2];
                p.process(&mut io, 64);
                latency.store(p.latency(), Ordering::Relaxed);
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
    wait(
        "the plugin to process",
        Box::new({
            let last = Arc::clone(&last);
            move || (f32::from_bits(last.load(Ordering::Relaxed)) - 1.0).abs() < 1e-6
        }),
    );
    // Mode "C" reaching `process` makes the plugin ask for a latency change.
    tx.send(("3", 2.0)).expect("audio thread");
    wait(
        "the restart",
        Box::new({
            let latency = Arc::clone(&latency);
            move || {
                svc.service();
                latency.load(Ordering::Relaxed) == 7
            }
        }),
    );
    wait(
        "the plugin to process again",
        Box::new({
            let last = Arc::clone(&last);
            move || (f32::from_bits(last.load(Ordering::Relaxed)) - 1.0).abs() < 1e-6
        }),
    );
    stop.store(true, Ordering::Relaxed);
    let (p, seen) = audio.join().expect("audio thread");
    assert!(!p.failed(), "a restart is not a failure");
    assert_eq!(p.latency(), 7);
    assert!(
        seen.iter().all(|&v| [0.5, 1.0].iter().any(|w| (v - w).abs() < 1e-6)),
        "only the plugin's sound or the passed-through input is heard: {seen:?}"
    );
}

#[test]
// Contract: a parameter's shown value comes from the live plugin, not from its advertised default
// (the gain declares 1.0 and starts at 0.5, like Surge XT's ~800 parameters at 0).
fn live_param_is_the_plugins_value_not_its_advertised_default() {
    let info = soundcraft_vst3_host::plugin_info(&gain_id()).expect("info");
    assert_eq!(info.param("7").map(|p| p.default), Some(1.0), "advertised");
    let mut p = gain();
    let mut svc = p.service_handle().expect("service handle");
    assert_eq!(svc.live_param("7"), Some(0.5), "live");
    assert_eq!(svc.live_param("no-such"), None);
}
