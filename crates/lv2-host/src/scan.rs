//! Reading what an installed LV2 plugin is from its bundle and classes (safe, no plugin code
//! runs): SoundCraft's category and instrument flag, and the bundle folder. The lilv reading of
//! parameters lives in `meta` (native only).

use soundcraft_dsp::Category;
use std::path::PathBuf;

/// One parameter of a plugin: the control input port it drives and, for an enumeration, the
/// plugin value of each choice (index `i` is `scale[i]`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamPort {
    pub port: usize,
    pub scale: Vec<f32>,
}

/// Which control ports drive which parameter, and the designated ports the host sets itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct PortMap {
    /// One entry per `PluginInfo::params` entry, in order.
    pub params: Vec<ParamPort>,
    /// `lv2:enabled` control input: set to 1 (not a parameter).
    pub enabled: Option<usize>,
    /// `lv2:freeWheeling` control input: set to 0 (not a parameter).
    pub freewheel: Option<usize>,
    /// The control output that reports the plugin's latency in samples.
    pub latency: Option<usize>,
}

/// SoundCraft's insert category for a plugin with these LV2 classes (labels, parents included).
/// Instruments are `Category::Instrument` whatever their classes.
pub fn category_from_classes<S: AsRef<str>>(classes: &[S], is_instrument: bool) -> Category {
    let has = |names: &[&str]| classes.iter().any(|c| names.contains(&c.as_ref()));
    if is_instrument {
        Category::Instrument
    } else if has(&["EQ Plugin", "Filter Plugin"]) {
        Category::Eq
    } else if has(&["Compressor Plugin", "Dynamics Plugin", "Limiter Plugin", "Gate Plugin", "Expander Plugin"]) {
        Category::Dynamics
    } else if has(&["Reverb Plugin"]) {
        Category::Reverb
    } else if has(&["Delay Plugin"]) {
        Category::Delay
    } else if has(&["Modulator Plugin", "Chorus Plugin", "Flanger Plugin", "Phaser Plugin"]) {
        Category::Modulation
    } else if has(&["Distortion Plugin", "Amplifier Plugin", "Simulator Plugin"]) {
        Category::Harmonic
    } else if has(&["Pitch Plugin"]) {
        Category::PitchShift
    } else {
        Category::Other
    }
}

/// True for an instrument: the LV2 `Instrument Plugin` class, or no audio input with a MIDI/atom
/// input (a generator that is driven by events).
pub fn is_instrument<S: AsRef<str>>(classes: &[S], audio_inputs: usize, atom_inputs: usize) -> bool {
    classes.iter().any(|c| c.as_ref() == "Instrument Plugin") || (audio_inputs == 0 && atom_inputs > 0)
}

/// The bundle folder of a plugin from its bundle URI (`file:///usr/lib/lv2/x.lv2/`). A URI that is
/// not a `file:` URI is kept as it is.
pub fn bundle_path(uri: &str) -> PathBuf {
    match uri.strip_prefix("file://") {
        Some(rest) => PathBuf::from(percent_decode(rest).trim_end_matches('/')),
        None => PathBuf::from(uri),
    }
}

/// Decodes `%XX` escapes (a malformed escape is kept as written).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex =
            if c == b'%' { b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) } else { None };
        match hex {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lv2_classes_map_to_categories() {
        let cat = |classes: &[&str]| category_from_classes(classes, false);
        assert_eq!(cat(&["Reverb Plugin", "Plugin"]), Category::Reverb);
        assert_eq!(cat(&["Delay Plugin"]), Category::Delay);
        assert_eq!(cat(&["EQ Plugin"]), Category::Eq);
        assert_eq!(cat(&["Filter Plugin", "Plugin"]), Category::Eq);
        assert_eq!(cat(&["Compressor Plugin", "Dynamics Plugin"]), Category::Dynamics);
        assert_eq!(cat(&["Limiter Plugin"]), Category::Dynamics);
        assert_eq!(cat(&["Gate Plugin"]), Category::Dynamics);
        assert_eq!(cat(&["Expander Plugin"]), Category::Dynamics);
        assert_eq!(cat(&["Modulator Plugin", "Chorus Plugin"]), Category::Modulation);
        assert_eq!(cat(&["Flanger Plugin"]), Category::Modulation);
        assert_eq!(cat(&["Phaser Plugin"]), Category::Modulation);
        assert_eq!(cat(&["Distortion Plugin"]), Category::Harmonic);
        assert_eq!(cat(&["Amplifier Plugin"]), Category::Harmonic);
        assert_eq!(cat(&["Simulator Plugin"]), Category::Harmonic);
        assert_eq!(cat(&["Pitch Plugin"]), Category::PitchShift);
        assert_eq!(cat(&["Utility Plugin"]), Category::Other);
        assert_eq!(category_from_classes::<&str>(&[], false), Category::Other);
        assert_eq!(category_from_classes(&["Reverb Plugin"], true), Category::Instrument);
    }

    #[test]
    fn instruments_are_the_class_or_a_generator_with_events() {
        assert!(is_instrument(&["Instrument Plugin", "Plugin"], 0, 1));
        assert!(is_instrument(&["Generator Plugin"], 0, 1));
        assert!(!is_instrument(&["Generator Plugin"], 0, 0));
        assert!(!is_instrument(&["Reverb Plugin"], 2, 1));
    }

    #[test]
    fn bundle_uris_become_paths() {
        assert_eq!(bundle_path("file:///usr/lib/lv2/calf.lv2/"), PathBuf::from("/usr/lib/lv2/calf.lv2"));
        assert_eq!(bundle_path("file:///home/u/My%20Plugs/x.lv2/"), PathBuf::from("/home/u/My Plugs/x.lv2"));
        assert_eq!(bundle_path("file:///a%zz/b"), PathBuf::from("/a%zz/b"));
        assert_eq!(bundle_path("http://example.org/x.lv2/"), PathBuf::from("http://example.org/x.lv2/"));
    }
}
