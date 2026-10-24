//! SoundCraft's LV2 plugin host.
//!
//! Lists the installed LV2 plugins (through the system `lilv`, via `livi`) and exposes each
//! audio effect as a [`soundcraft_dsp::Plugin`], so the mix engine treats LV2 plugins like the
//! other hosted ones. Plugin ids are `lv2:<plugin URI>`. Parameters are the plugin's control
//! input ports, with the port symbol as the parameter id and the port name as its name.
//!
//! This is SoundCraft's isolated `unsafe` crate (craftrules never-crash), in the same shape as
//! `soundcraft-clap-host`: `unsafe` is denied crate-wide and allowed only in the private `ffi`
//! module, where the two calls that run plugin code (instantiate, run) live. The public API is
//! safe, returns `Result`/`Option` and never panics. livi itself uses `unwrap` internally, which
//! we cannot change. A plugin that crashes takes the process with it: native code is not sandboxed.
//!
//! What is hosted: audio effects with audio inputs and outputs (mono, stereo, or the channel
//! rules in `plugin.rs`). Not yet: editors (the generic parameter editor works), LV2 state
//! extensions (SoundCraft stores the control values itself), MIDI and other atom events,
//! CV ports, and instruments beyond what the mixer already sends.
//!
//! `PluginInfo` must be `&'static`: each plugin gets one leaked `PluginInfo` (with its parameter
//! table) the first time it is asked for, and the registry keeps it. The leak is bounded by the
//! number of distinct installed plugins actually used.
//!
//! On `wasm32` the crate compiles to stubs: scans are empty and nothing can be created.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// On wasm the port-map types are only used by the native host.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub mod scan;

#[cfg(not(target_arch = "wasm32"))]
#[allow(unsafe_code)]
mod ffi;
#[cfg(not(target_arch = "wasm32"))]
mod meta;
#[cfg(not(target_arch = "wasm32"))]
mod plugin;
#[cfg(not(target_arch = "wasm32"))]
mod registry;

#[cfg(not(target_arch = "wasm32"))]
pub use plugin::Lv2Plugin;

use soundcraft_dsp::{Category, Plugin, PluginInfo};

/// Prefix of every hosted-LV2 plugin id.
pub const ID_PREFIX: &str = "lv2:";

/// The LV2 plugin URI inside a `lv2:<uri>` SoundCraft id (`None` for other ids, an empty URI, or
/// one with a NUL byte).
pub fn parse_id(id: &str) -> Option<&str> {
    id.strip_prefix(ID_PREFIX).filter(|s| !s.is_empty() && !s.contains('\0'))
}

/// One plugin found by a scan.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Lv2Descriptor {
    /// SoundCraft id: `lv2:<plugin URI>`.
    pub id: String,
    /// The LV2 plugin URI (e.g. `urn:example:reverb`).
    pub uri: String,
    pub name: String,
    /// The plugin's author, as its bundle names it (empty when it names none).
    pub vendor: String,
    pub category: Category,
    pub is_instrument: bool,
    /// The LV2 class labels of the plugin, parents included (e.g. "Reverb Plugin").
    pub classes: Vec<String>,
    /// The bundle folder the plugin lives in.
    pub path: std::path::PathBuf,
}

/// Why an LV2 operation failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Lv2Error {
    #[error("no LV2 plugin `{0}`")]
    NotFound(String),
    #[error("cannot create LV2 plugin `{0}`: {1}")]
    Instantiate(String, String),
    #[error("LV2 plugins are not supported on this platform")]
    Unsupported,
}

#[cfg(not(target_arch = "wasm32"))]
mod api {
    use super::*;

    pub fn cached() -> Option<Vec<Lv2Descriptor>> {
        registry::cached()
    }
    pub fn scan() -> Vec<Lv2Descriptor> {
        registry::scan()
    }
    pub fn rescan() -> Vec<Lv2Descriptor> {
        registry::rescan()
    }
    pub fn shutdown() {
        registry::shutdown()
    }
    pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Lv2Error> {
        registry::instantiate(id)
    }
    pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
        registry::plugin_info(id)
    }
}

#[cfg(target_arch = "wasm32")]
mod api {
    use super::*;

    pub fn cached() -> Option<Vec<Lv2Descriptor>> {
        Some(Vec::new())
    }
    pub fn scan() -> Vec<Lv2Descriptor> {
        Vec::new()
    }
    pub fn rescan() -> Vec<Lv2Descriptor> {
        Vec::new()
    }
    pub fn shutdown() {}
    pub fn instantiate(_id: &str) -> Result<Box<dyn Plugin>, Lv2Error> {
        Err(Lv2Error::Unsupported)
    }
    pub fn plugin_info(_id: &str) -> Option<&'static PluginInfo> {
        None
    }
}

/// Every installed LV2 audio effect and instrument (the LV2 path and lilv's standard folders).
/// Scanned on first call, then cached in memory. Creates no plugin instances.
pub fn scan() -> Vec<Lv2Descriptor> {
    api::scan()
}

/// Forgets the cached scan and scans again (a newly installed bundle appears).
pub fn rescan() -> Vec<Lv2Descriptor> {
    api::rescan()
}

/// The plugins of the last scan, without scanning: `None` until something has scanned.
pub fn cached() -> Option<Vec<Lv2Descriptor>> {
    api::cached()
}

/// Drops the lilv world and the scan cache. Plugin instances already created keep working; call
/// it once at exit, after the instances are gone.
pub fn shutdown() {
    api::shutdown()
}

/// Creates a plugin by `lv2:<uri>`, prepared for 48 kHz stereo with 1024-frame blocks (like
/// `soundcraft_dsp::create`), with the reason when it fails.
pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Lv2Error> {
    api::instantiate(id)
}

/// Creates a plugin by `lv2:<uri>`; `None` if unknown or it fails to load.
pub fn create(id: &str) -> Option<Box<dyn Plugin>> {
    parse_id(id)?;
    match api::instantiate(id) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("{e}");
            None
        }
    }
}

/// The description of a `lv2:<uri>` plugin: its name, category and parameters. The first call
/// for a plugin reads its metadata; the result is cached (and leaked, see the crate docs).
pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    api::plugin_info(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse() {
        assert_eq!(parse_id("lv2:http://example.org/plugins/reverb"), Some("http://example.org/plugins/reverb"));
        assert_eq!(parse_id("lv2:"), None);
        assert_eq!(parse_id("clap:com.u-he.diva"), None);
        assert_eq!(parse_id("eq_7band"), None);
        assert_eq!(parse_id("LV2:x"), None);
        assert_eq!(parse_id("lv2:a\0b"), None);
    }

    #[test]
    fn non_lv2_ids_are_rejected_without_scanning() {
        assert!(create("eq_7band").is_none());
        assert!(create("lv2:").is_none());
        assert!(plugin_info("reverb").is_none());
        assert!(instantiate("clap:x").is_err());
    }

    #[test]
    fn unknown_uri_is_not_found_not_a_crash() {
        assert!(create("lv2:urn:soundcraft:no-such-plugin").is_none());
        assert!(plugin_info("lv2:urn:soundcraft:no-such-plugin").is_none());
        assert!(matches!(instantiate("lv2:urn:soundcraft:no-such-plugin"), Err(Lv2Error::NotFound(_))));
    }

    /// The installed-plugin tests: each names a bundle this machine has. A missing one is
    /// reported and skipped, never failed.
    #[cfg(not(target_arch = "wasm32"))]
    mod installed {
        use super::*;

        const TREMOLO: &str = "lv2:http://guitarix.sourceforge.net/plugins/gx_tremolo#_tremolo";
        const CHORUS: &str = "lv2:http://guitarix.sourceforge.net/plugins/gx_chorus_stereo#_chorus_stereo";
        const NAM: &str = "lv2:http://github.com/mikeoliphant/neural-amp-modeler-lv2";
        const SR: f32 = 48_000.0;

        /// True when the plugin is installed; otherwise says so and the test returns early.
        fn installed(id: &str) -> bool {
            let found = scan().iter().any(|d| d.id == id);
            if !found {
                eprintln!("SKIPPED: {id} is not installed");
            }
            found
        }

        /// Runs a 1 kHz sine of `seconds` through `p` on `channels` strip channels, in blocks of
        /// `block` frames, and returns the output.
        fn run_sine(p: &mut dyn Plugin, channels: usize, block: usize, seconds: usize) -> Vec<Vec<f32>> {
            let total = 48_000 * seconds;
            let sine: Vec<f32> = (0..total).map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / SR).sin()).collect();
            let mut io: Vec<Vec<f32>> = vec![sine; channels];
            p.prepare(SR, block, channels);
            for start in (0..total).step_by(block) {
                let end = (start + block).min(total);
                let mut chunk: Vec<Vec<f32>> = io.iter().map(|c| c[start..end].to_vec()).collect();
                p.process(&mut chunk, end - start);
                for (dst, src) in io.iter_mut().zip(chunk) {
                    dst[start..end].copy_from_slice(&src);
                }
            }
            io
        }

        fn assert_live(io: &[Vec<f32>], p: &dyn Plugin) {
            assert!(!p.failed(), "{} failed while processing", p.info().id);
            for ch in io {
                assert!(ch.iter().all(|x| x.is_finite()), "{}: non-finite output", p.info().id);
            }
        }

        #[test]
        fn mono_effect_runs_one_instance_per_channel_on_a_stereo_strip() {
            if !installed(TREMOLO) {
                return;
            }
            let mut p = instantiate(TREMOLO).expect("tremolo instantiates");
            let io = run_sine(p.as_mut(), 2, 64, 1);
            assert_live(&io, p.as_ref());
            assert!(io.iter().all(|ch| ch.iter().any(|x| x.abs() > 1e-4)), "tremolo output is silent");
        }

        #[test]
        fn stereo_effect_runs_directly_on_a_stereo_strip() {
            if !installed(CHORUS) {
                return;
            }
            let mut p = instantiate(CHORUS).expect("chorus instantiates");
            let io = run_sine(p.as_mut(), 2, 64, 1);
            assert_live(&io, p.as_ref());
            assert!(io.iter().all(|ch| ch.iter().any(|x| x.abs() > 1e-4)), "chorus output is silent");
        }

        #[test]
        fn neural_amp_modeler_instantiates_and_runs_mono_and_stereo_without_a_model() {
            if !installed(NAM) {
                return;
            }
            let mut mono = instantiate(NAM).expect("NAM instantiates");
            let io = run_sine(mono.as_mut(), 1, 64, 1);
            assert_live(&io, mono.as_ref());
            let mut stereo = instantiate(NAM).expect("NAM instantiates");
            let io = run_sine(stereo.as_mut(), 2, 64, 1);
            assert_live(&io, stereo.as_ref());
        }

        /// A NAM model file: a one-layer LSTM with 1 hidden unit (16 weights: 14 for the layer,
        /// then the head weight and head bias). Every weight is zero but the head bias, so the
        /// output is `head_bias` whatever the input. NeuralAudio accepts only WaveNet and LSTM
        /// models (no Linear), and ships no model here, so the test writes one.
        fn nam_lstm_model(dir: &std::path::Path, name: &str, head_bias: f32) -> std::path::PathBuf {
            let path = dir.join(name);
            let weights: Vec<String> = (0..15).map(|_| "0.0".to_string()).chain([head_bias.to_string()]).collect();
            let json = format!(
                r#"{{"version":"0.5.0","metadata":{{}},"architecture":"LSTM","config":{{"num_layers":1,"hidden_size":1}},"weights":[{}],"sample_rate":48000}}"#,
                weights.join(",")
            );
            std::fs::write(&path, json).expect("the model file is written");
            path
        }

        /// RMS of `x` over the frames from `from` on.
        fn rms_from(x: &[f32], from: usize) -> f32 {
            let tail = &x[from..];
            (tail.iter().map(|v| v * v).sum::<f32>() / tail.len().max(1) as f32).sqrt()
        }

        #[test]
        fn neural_amp_modeler_loads_a_model_set_as_a_file_parameter() {
            if !installed(NAM) {
                return;
            }
            let info = plugin_info(NAM).expect("NAM has a plugin info");
            let file = info.files.first().expect("NAM lists its model as a file parameter");
            assert_eq!(file.id, "http://github.com/mikeoliphant/neural-amp-modeler-lv2#model");
            let dir = std::env::temp_dir().join(format!("sc-lv2-nam-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let model = nam_lstm_model(&dir, "half.nam", 0.5);
            let mut p = instantiate(NAM).expect("NAM instantiates");
            p.prepare(SR, 64, 1);
            assert!(p.set_file(file.id, &model.to_string_lossy()), "NAM takes the model path");
            assert!(!p.set_file("urn:no-such#param", "/x"), "an unknown id is refused");
            let mut service = p.service_handle().expect("NAM runs its workers on the main thread");
            // Two seconds of 1 kHz at 48 kHz, in 64-frame blocks, servicing the worker between them.
            let total = 48_000 * 2;
            let sine: Vec<f32> = (0..total).map(|i| 0.25 * (std::f32::consts::TAU * 1000.0 * i as f32 / SR).sin()).collect();
            let mut out = Vec::with_capacity(total);
            for chunk in sine.chunks(64) {
                let mut io = vec![chunk.to_vec()];
                p.process(&mut io, chunk.len());
                out.extend_from_slice(&io[0]);
                service.service();
            }
            assert!(!p.failed(), "NAM failed with the model");
            assert!(out.iter().all(|x| x.is_finite()), "non-finite output");
            // The last second is after the model loaded: its output is the head bias, 0.5, with
            // no trace of the 1 kHz input (which would pass through unchanged without a model).
            let tail = &out[48_000..];
            let mean = tail.iter().sum::<f32>() / tail.len() as f32;
            let spread = (tail.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / tail.len() as f32).sqrt();
            eprintln!("NAM with the 0.5 head-bias model: mean {mean}, spread {spread}, input rms {}", rms_from(&sine, 48_000));
            assert!((mean - 0.5).abs() < 0.02, "expected the model's constant 0.5, got mean {mean}");
            assert!(spread < 0.02, "the output still follows the input (spread {spread})");
            // A re-prepare (new instances, as on a sample-rate change) gets the file again.
            p.prepare(SR, 128, 1);
            let mut service = p.service_handle().expect("NAM runs its workers on the main thread");
            let mut again = Vec::new();
            for chunk in sine[..48_000].chunks(128) {
                let mut io = vec![chunk.to_vec()];
                p.process(&mut io, chunk.len());
                again.extend_from_slice(&io[0]);
                service.service();
            }
            let tail = &again[24_000..];
            let mean = tail.iter().sum::<f32>() / tail.len() as f32;
            assert!((mean - 0.5).abs() < 0.02, "after a re-prepare the model is gone (mean {mean})");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn controls_round_trip_through_the_state_blob() {
            if !installed(TREMOLO) {
                return;
            }
            let mut p = instantiate(TREMOLO).expect("tremolo instantiates");
            let info = p.info();
            let first = info.params.first().expect("tremolo has parameters");
            let mid = first.min + (first.max - first.min) * 0.3;
            assert!(p.set_param(first.id, mid));
            let blob = p.save_state().expect("LV2 hosts always save their controls");
            assert!(blob.starts_with(b"SCL2"));
            let mut q = instantiate(TREMOLO).expect("tremolo instantiates");
            assert!(q.load_state(&blob));
            assert_eq!(q.param(first.id), p.param(first.id));
        }

        /// Regression: two threads making and freeing instances of one plugin crashed in the
        /// plugin's own code (guitarix's plugins; a segfault in `lv2_descriptor` during
        /// instantiate). Making and freeing now run under one lock (`ffi::free_instances`).
        #[test]
        fn two_threads_making_and_freeing_one_plugin_do_not_crash() {
            if !installed(TREMOLO) {
                return;
            }
            let churn: Vec<_> = (0..2)
                .map(|_| {
                    std::thread::spawn(|| {
                        for _ in 0..300 {
                            drop(instantiate(TREMOLO).expect("tremolo instantiates"));
                        }
                    })
                })
                .collect();
            for h in churn {
                h.join().expect("the churn thread finishes");
            }
        }

        /// One instance keeps running (as on the audio thread) while another is made and freed.
        #[test]
        fn instances_of_one_plugin_can_be_made_and_freed_while_another_runs() {
            if !installed(TREMOLO) {
                return;
            }
            let mut keep = instantiate(TREMOLO).expect("tremolo instantiates");
            keep.prepare(SR, 64, 2);
            let churn = std::thread::spawn(|| {
                for _ in 0..100 {
                    drop(instantiate(TREMOLO).expect("tremolo instantiates"));
                }
            });
            let mut io = vec![vec![0.1f32; 64]; 2];
            while !churn.is_finished() {
                keep.process(&mut io, 64);
            }
            churn.join().expect("the churn thread finishes");
            assert_live(&io, keep.as_ref());
        }

        #[test]
        fn bad_state_is_refused_or_ignored_not_applied() {
            if !installed(TREMOLO) {
                return;
            }
            let mut p = instantiate(TREMOLO).expect("tremolo instantiates");
            let first = p.info().params.first().expect("tremolo has parameters");
            let before = p.param(first.id);
            assert!(!p.load_state(b"XXXX first=1\n"), "bad magic is refused");
            assert!(!p.load_state(b""), "empty blob is refused");
            let blob = format!("SCL2nosuch.symbol=1\n{}=NaN\n{}=inf\n", first.id, first.id);
            assert!(p.load_state(blob.as_bytes()), "a well-formed blob is accepted");
            assert_eq!(p.param(first.id), before, "NaN and infinite values are ignored");
        }
    }
}
