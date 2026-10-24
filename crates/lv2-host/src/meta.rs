//! The LV2 parameters of a plugin, read with lilv through livi (native only): one `ParamInfo`
//! per control input port, and the port map that drives them.

use crate::scan::{ParamPort, PortMap};
use livi::World;
use soundcraft_dsp::{ParamInfo, Taper, Unit};

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

/// Index of the scale value closest to `value` (0 when there is none).
fn nearest_index(scale: &[f32], value: f32) -> f32 {
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
