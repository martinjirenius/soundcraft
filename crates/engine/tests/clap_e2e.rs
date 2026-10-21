//! A hosted CLAP plugin through the whole stack: `mix.insert` / `mix.insert_param` accept a
//! `clap:` id and the mix engine renders through it. Uses the test plugin built from
//! `crates/clap-host/tests/fixture` (a gain whose param "7" is linear gain).

use serde_json::json;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
use soundcraft_time::Range;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn install_fixture() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(install_fixture_now);
}

fn install_fixture_now() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clap-fixture-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .env_remove("RUSTFLAGS")
        .status()
        .unwrap();
    assert!(st.success());
    let lib = if cfg!(target_os = "macos") {
        "libclap_fixture.dylib"
    } else if cfg!(windows) {
        "clap_fixture.dll"
    } else {
        "libclap_fixture.so"
    };
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("engine-clap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target.join("debug").join(lib), dir.join("Fixture.clap")).unwrap();
    assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 6);
}

#[test]
fn clap_insert_renders_through_the_mix() {
    install_fixture();
    let mut e = Engine::default();
    let list = e.execute("engine.clap_plugins", &json!({})).unwrap();
    assert!(list.as_array().unwrap().iter().any(|d| d["id"] == "clap:org.soundcraft.test.gain"));

    let s = e.session_mut();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.pool.insert(SourceId(77), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4800]] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "dc", SourceId(77), 0, 0, 4800));
    let dry = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][2000];
    assert!(dry.abs() > 0.01);

    let r = e.execute("mix.insert", &json!({"track": t.0, "plugin": "clap:org.soundcraft.test.gain", "params": {"7": 2.0}})).unwrap();
    assert_eq!(r["plugin"], "clap:org.soundcraft.test.gain");
    let wet = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][2000];
    assert!((wet - 2.0 * dry).abs() < 1e-4, "dry {dry} wet {wet}");

    e.execute("mix.insert_param", &json!({"track": t.0, "slot": 0, "param": "7", "value": 0.5})).unwrap();
    let half = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][2000];
    assert!((half - 0.5 * dry).abs() < 1e-4, "dry {dry} half {half}");
    assert!(e.execute("mix.insert_param", &json!({"track": t.0, "slot": 0, "param": "Gain", "value": 1.0})).is_err());
}

#[test]
fn a_refused_state_is_reported() {
    install_fixture();
    let gain = "clap:org.soundcraft.test.gain";
    let make = |state: Option<&[u8]>| soundcraft_mix::create_instance(gain, state, 48_000.0, 512, 2).unwrap();
    let saved = make(None).plugin.save_state().unwrap();
    assert!(!make(None).refused_state);
    assert!(!make(Some(&saved)).refused_state);
    assert!(make(Some(&[0xff; 16])).refused_state, "NaN gain");
}

#[test]
fn clap_state_is_captured_stored_and_restored_before_the_first_block() {
    install_fixture();
    let gain = "clap:org.soundcraft.test.gain";
    let mut e = Engine::default();
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.pool.insert(SourceId(78), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4800]] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "dc", SourceId(78), 0, 0, 4800));
    e.execute("mix.insert", &json!({"track": t.0, "plugin": gain, "params": {"7": 2.0}})).unwrap();

    // A realtime-style engine: the instance is created "on the UI thread" from the specs and
    // adopted; its live state (gain 1.5, changed behind the session's back as a plugin GUI
    // would) is captured and stored on the insert.
    let mut eng = soundcraft_mix::MixEngine::new(48_000.0, 512);
    eng.set_external_instances(true);
    let specs = soundcraft_mix::instance_specs(e.session());
    assert_eq!(specs.len(), 1);
    let mut p = soundcraft_mix::create_instance(&specs[0].id, None, 48_000.0, 512, specs[0].ch).unwrap().plugin;
    p.set_param("7", 1.5);
    let mut io = vec![vec![0.0f32; 64]];
    p.process(&mut io, 64);
    let mut reader = p.state_reader().unwrap();
    eng.adopt(vec![soundcraft_mix::PreparedInstance { track: t, slot: 0, id: gain.into(), ch: 1, plugin: p }]);
    eng.ensure_synced(e.session());
    let blob = reader.save_state().unwrap();
    e.execute("mix.insert_state", &json!({"track": t.0, "slot": 0, "state": soundcraft_model::b64::encode(&blob)})).unwrap();
    assert_eq!(eng.take_retired().len(), 0);

    // Offline render (synchronous creation): the state is restored before the first block.
    // With the gain absent from the session's params, the state's 1.5 is what plays.
    let gain_of = |s: &soundcraft_model::Session| soundcraft_mix::render_range(s, Range::new(0, 4800), 512)[0][2000];
    let mut s2 = e.session().clone();
    s2.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.remove("7");
    let restored = gain_of(&s2);
    let mut s3 = e.session().clone();
    let ins = s3.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap();
    ins.state = None;
    ins.params.insert("7".into(), 1.5);
    let reference = gain_of(&s3);
    assert!((restored - reference).abs() < 1e-5, "state {restored} vs param 1.5 {reference}");
    // Session params apply on top of the state.
    e.execute("mix.insert_param", &json!({"track": t.0, "slot": 0, "param": "7", "value": 0.5})).unwrap();
    let half = gain_of(e.session());
    assert!((half - reference / 3.0).abs() < 1e-5, "{half} vs {}", reference / 3.0);
}

/// Regression: Surge XT played digital silence because placing a plugin stored every parameter's
/// advertised default (0 for Surge) and the mixer pushed them all in. The fixture synth does the
/// same trap: its Volume really starts at 1.0 but advertises 0.0. Contract: with no `params`,
/// nothing is stored and the host never writes a parameter, so the plugin keeps its own value.
#[test]
fn an_instrument_placed_without_params_keeps_the_plugins_own_values() {
    install_fixture();
    let mut e = Engine::default();
    let r = e.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    let track = r["tracks"][0].as_u64().unwrap();
    e.execute("mix.instrument", &json!({"track": track, "plugin": "clap:org.soundcraft.test.synth"})).unwrap();
    let r = e.execute("midi.new_clip", &json!({"track": track, "start": 0, "length": 96_000})).unwrap();
    let clip = r["clip"].as_u64().unwrap();
    e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": 0, "length_ticks": 960, "velocity": 127})).unwrap();

    let out = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512);
    let peak = out.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak > 0.5, "the synth plays at its own volume, not the advertised default 0: peak {peak}");
    let ins = e.session().tracks.iter().find(|t| t.id.0 == track).and_then(|t| t.instrument.as_ref()).unwrap();
    assert!(ins.params.is_empty(), "nothing was set, so nothing is stored: {:?}", ins.params);
}

/// Contract: automation on a parameter that is not stored (the plugin owns it) still reaches the plugin.
#[test]
fn automation_on_an_unset_plugin_param_reaches_the_plugin() {
    install_fixture();
    let mut e = Engine::default();
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    s.pool.insert(SourceId(79), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4800]] })));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "dc", SourceId(79), 0, 0, 4800));
    e.execute("mix.insert", &json!({"track": t.0, "plugin": "clap:org.soundcraft.test.gain"})).unwrap();
    let plain = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][2000];
    assert!(plain.abs() > 0.1, "the plugin keeps its own gain 1.0: {plain}");
    for at in [0, 4800] {
        e.execute("automation.set_point", &json!({"track": t.0, "param": "plugin:a:7", "at": at, "value": 0.5})).unwrap();
    }
    let automated = soundcraft_mix::render_range(e.session(), Range::new(0, 4800), 512)[0][2000];
    assert!((automated - 0.5 * plain).abs() < 1e-4, "automation drives the unset gain: {automated}");
}
