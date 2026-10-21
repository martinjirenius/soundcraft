//! Protects the invariant "the host never writes a parameter value that no user action, automation
//! or saved session set": re-activating a plugin re-sends only the values the host set (a value
//! only the plugin set is the plugin's own), and a loaded state gives every value back to the
//! plugin. The count comes from the fixture's own counter, which only this file reads. It has its
//! own test binary because the counter is process-wide.

// The crate denies `unsafe` except in `ffi`. The only unsafe here loads the fixture library a
// second time (dlopen of the same file returns the same image) to read its exported counter.
#![allow(unsafe_code)]

mod common;

#[test]
fn reactivation_resends_only_the_values_the_host_set() {
    let dir = common::fixture_dir();
    let id = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
    // SAFETY: the library is the fixture the host already loaded from this same path; loading it
    // runs no code that this test relies on, and it stays loaded while `lib` lives.
    let lib = unsafe { libloading::Library::new(common::fixture_binary()) }.expect("load the fixture");
    // SAFETY: the fixture exports `fixture_param_points` as `extern "C" fn() -> u64` (tests/fixture).
    let read: extern "C" fn() -> u64 = *unsafe { lib.get::<extern "C" fn() -> u64>(b"fixture_param_points\0") }.expect("fixture counter");

    let mut p = soundcraft_vst3_host::create(&id).expect("instantiate");
    let mut io = vec![vec![0.5f32; 64]; 2];
    p.prepare(48_000.0, 64, 2);
    p.process(&mut io, 64);

    // Nothing set by the host: a re-activation sends the plugin nothing.
    let before = read();
    p.prepare(48_000.0, 64, 2);
    p.process(&mut io, 64);
    assert_eq!(read() - before, 0, "a re-activation re-sent values the host never set");

    // One host edit: a re-activation re-sends exactly that parameter.
    assert!(p.set_param("7", 1.5));
    let before = read();
    p.prepare(48_000.0, 64, 2);
    p.process(&mut io, 64);
    assert_eq!(read() - before, 1, "a re-activation must re-send the one host-set parameter");

    // A loaded state is the plugin's own again: nothing is re-sent after it.
    let blob = p.save_state().expect("state");
    assert!(p.load_state(&blob));
    let before = read();
    p.prepare(48_000.0, 64, 2);
    p.process(&mut io, 64);
    assert_eq!(read() - before, 0, "a re-activation after a state load re-sent host values over the loaded state");
}
