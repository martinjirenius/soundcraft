//! The LV2 parameters of a plugin, read with lilv through livi (native only): one `ParamInfo`
//! per control input port, and the port map that drives them.

use crate::scan::{ParamPort, PortMap, UiInfo};
use livi::World;
use soundcraft_dsp::{FileParamInfo, ParamInfo, Taper, Unit};
use std::ffi::CString;

/// Most labels read for an enumeration parameter (as in the CLAP host).
const MAX_CHOICES: usize = 128;

const LV2_TOGGLED: &str = "http://lv2plug.in/ns/lv2core#toggled";
const LV2_CONTROL_PORT: &str = "http://lv2plug.in/ns/lv2core#ControlPort";
const LV2_INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const LV2_OUTPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#OutputPort";
const LV2_ENUMERATION: &str = "http://lv2plug.in/ns/lv2core#enumeration";
const LV2_LOGARITHMIC: &str = "http://lv2plug.in/ns/lv2core#logarithmic";
const LV2_DESIGNATION: &str = "http://lv2plug.in/ns/lv2core#designation";
const LV2_LATENCY: &str = "http://lv2plug.in/ns/lv2core#latency";
const LV2_ENABLED: &str = "http://lv2plug.in/ns/lv2core#enabled";
const LV2_FREEWHEELING: &str = "http://lv2plug.in/ns/lv2core#freeWheeling";
const LV2_ATOM_PORT: &str = "http://lv2plug.in/ns/ext/atom#AtomPort";
const ATOM_SUPPORTS: &str = "http://lv2plug.in/ns/ext/atom#supports";
const ATOM_PATH: &str = "http://lv2plug.in/ns/ext/atom#Path";
const PATCH_WRITABLE: &str = "http://lv2plug.in/ns/ext/patch#writable";
const PATCH_MESSAGE: &str = "http://lv2plug.in/ns/ext/patch#Message";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const LV2_X11_UI: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";

/// A lilv number as a finite `f32` (floats and integers; anything else is `None`).
fn node_f32(n: Option<&livi::lilv::node::Node>) -> Option<f32> {
    let n = n?;
    let v = if n.is_float() {
        n.as_float()?
    } else if n.is_int() {
        n.as_int()? as f32
    } else {
        return None;
    };
    v.is_finite().then_some(v)
}

/// The LV2 parameters of a plugin, as SoundCraft sees them (one `ParamInfo` per control input
/// port that is not designated for the host), with the port map that drives them. The strings
/// are leaked: they live as long as the process, like every `PluginInfo` (see the crate docs).
pub(crate) fn params(plugin: &livi::Plugin, world: &World) -> (Vec<ParamInfo>, PortMap) {
    let raw_world = world.raw();
    let raw = plugin.raw();
    let node = |uri: &str| raw_world.new_uri(uri);
    let (control, input, output) = (node(LV2_CONTROL_PORT), node(LV2_INPUT_PORT), node(LV2_OUTPUT_PORT));
    let (toggled, enumeration, logarithmic, designation) = (node(LV2_TOGGLED), node(LV2_ENUMERATION), node(LV2_LOGARITHMIC), node(LV2_DESIGNATION));
    let mut map = PortMap::default();
    let mut params = Vec::new();
    for lp in raw.iter_ports() {
        if !lp.is_a(&control) {
            continue;
        }
        let index = lp.index();
        let designated = |uri: &str| lp.value(&designation).iter().any(|n| n.as_uri() == Some(uri));
        let symbol = lp.symbol().and_then(|s| s.as_str().map(str::to_string)).unwrap_or_default();
        if lp.is_a(&output) {
            if designated(LV2_LATENCY) || (map.latency.is_none() && symbol == "latency") {
                map.latency = Some(index);
            }
            continue;
        }
        if !lp.is_a(&input) || symbol.is_empty() {
            continue;
        }
        if designated(LV2_ENABLED) {
            map.enabled = Some(index);
            continue;
        }
        if designated(LV2_FREEWHEELING) {
            map.freewheel = Some(index);
            continue;
        }
        let range = lp.range();
        let (min, max) = match (node_f32(range.minimum.as_ref()), node_f32(range.maximum.as_ref())) {
            (Some(a), Some(b)) if a < b => (a, b),
            _ => (0.0, 1.0),
        };
        let default = node_f32(range.default.as_ref()).unwrap_or(0.0).clamp(min, max);
        let name = lp.name().and_then(|n| n.as_str().map(str::to_string)).filter(|n| !n.is_empty()).unwrap_or_else(|| symbol.clone());
        let mut points: Vec<(f32, String)> = lp
            .scale_points()
            .iter()
            .filter_map(|sp| {
                let value = node_f32(Some(&sp.value()))?;
                Some((value, sp.label().as_str().map(str::to_string).unwrap_or_default()))
            })
            .take(MAX_CHOICES + 1)
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut unit, mut taper) = (Unit::None, Taper::Linear);
        let (mut lo, mut hi, mut def) = (min, max, default);
        let mut scale: Vec<f32> = Vec::new();
        let mut choices: Vec<&'static str> = Vec::new();
        if lp.has_property(&toggled) {
            unit = Unit::Toggle;
            (lo, hi) = (0.0, 1.0);
            def = if default >= 0.5 { 1.0 } else { 0.0 };
        } else if lp.has_property(&enumeration) && (2..=MAX_CHOICES).contains(&points.len()) {
            unit = Unit::Choice;
            scale = points.iter().map(|(v, _)| *v).collect();
            choices = points.iter().map(|(v, label)| leak(if label.is_empty() { format!("{v}") } else { label.clone() })).collect();
            (lo, hi) = (0.0, (scale.len() - 1) as f32);
            def = nearest_index(&scale, default);
        } else if lp.has_property(&logarithmic) && min > 0.0 {
            taper = Taper::Log;
        }
        params.push(ParamInfo {
            id: leak(symbol),
            name: leak(name),
            min: lo,
            max: hi,
            default: def.clamp(lo, hi),
            unit,
            taper,
            choices: Box::leak(choices.into_boxed_slice()),
        });
        map.params.push(ParamPort { port: index, scale });
    }
    if map.latency.is_none() {
        map.latency = raw.latency_port_index();
    }
    (params, map)
}

/// The file parameters of a plugin: its `patch:writable` properties ranged `atom:Path` (a
/// property with no range at all also counts when it has a label and the plugin takes
/// `patch:Message`, as NAM's model does in some bundles). Only listed when the plugin has an atom
/// input for `patch:Message`, the port the host delivers them to; that port's ordinal goes into
/// `map.patch_input`. The strings are leaked like the parameters'. Extensions are not read: none
/// are known generically.
pub(crate) fn files(plugin: &livi::Plugin, world: &World, map: &mut PortMap) -> Vec<FileParamInfo> {
    let raw_world = world.raw();
    let raw = plugin.raw();
    let (atom_port, input, supports) = (raw_world.new_uri(LV2_ATOM_PORT), raw_world.new_uri(LV2_INPUT_PORT), raw_world.new_uri(ATOM_SUPPORTS));
    let message = raw_world.new_uri(PATCH_MESSAGE);
    let mut ordinal = 0usize;
    for lp in raw.iter_ports() {
        if !(lp.is_a(&atom_port) && lp.is_a(&input)) {
            continue;
        }
        let takes_message = lp.value(&supports).iter().any(|n| n.as_uri() == message.as_uri());
        if takes_message && map.patch_input.is_none() {
            map.patch_input = Some(ordinal);
        }
        ordinal += 1;
    }
    if map.patch_input.is_none() {
        return Vec::new();
    }
    let (writable, range, label, path) =
        (raw_world.new_uri(PATCH_WRITABLE), raw_world.new_uri(RDFS_RANGE), raw_world.new_uri(RDFS_LABEL), raw_world.new_uri(ATOM_PATH));
    let mut files = Vec::new();
    for prop in raw.value(&writable).iter() {
        let Some(uri) = prop.as_uri() else { continue };
        let ranges: Vec<String> = world.raw().find_nodes(Some(&prop), &range, None).iter().filter_map(|n| n.as_uri().map(str::to_string)).collect();
        let labelled =
            world.raw().find_nodes(Some(&prop), &label, None).iter().find_map(|n| n.as_str().map(str::to_string)).filter(|s| !s.is_empty());
        let accepted = if ranges.is_empty() { labelled.is_some() } else { ranges.iter().any(|r| Some(r.as_str()) == path.as_uri()) };
        if !accepted {
            continue;
        }
        let name = labelled.unwrap_or_else(|| uri.rsplit(['#', '/']).next().unwrap_or(uri).to_string());
        files.push(FileParamInfo { id: leak(uri.to_string()), name: leak(name), extensions: &[] });
    }
    files
}

/// The plugin's X11 UI (its first `ui:X11UI`), with the binary and bundle paths. None when it has
/// no X11 UI, or a piece of its path cannot be read (a UI this host cannot load is not offered).
pub(crate) fn x11_ui(plugin: &livi::Plugin, world: &World) -> Option<UiInfo> {
    let x11 = world.raw().new_uri(LV2_X11_UI);
    let ui = plugin.raw().uis()?.iter().find(|ui| ui.is_a(&x11))?;
    let uri = ui.uri().as_uri()?.to_string();
    let binary = ui.binary_uri()?.path()?.1;
    let bundle = ui.bundle_uri()?.path()?.1;
    let bundle = if bundle.ends_with('/') { bundle } else { format!("{bundle}/") };
    Some(UiInfo { uri: CString::new(uri).ok()?, binary: binary.into(), bundle: CString::new(bundle).ok()? })
}

/// Index of the scale value closest to `value` (0 when there is none).
pub(crate) fn nearest_index(scale: &[f32], value: f32) -> f32 {
    let mut best = (0usize, f32::INFINITY);
    for (i, s) in scale.iter().enumerate() {
        let d = (s - value).abs();
        if d < best.1 {
            best = (i, d);
        }
    }
    best.0 as f32
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_scale_point_is_the_default_index() {
        assert_eq!(nearest_index(&[0.0, 0.5, 1.0], 0.6), 1.0);
        assert_eq!(nearest_index(&[10.0, 20.0], -5.0), 0.0);
        assert_eq!(nearest_index(&[], 3.0), 0.0);
    }
}
