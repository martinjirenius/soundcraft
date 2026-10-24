//! The editor of a hosted LV2 plugin that has an X11 UI (`ui:X11UI`): the UI is loaded and
//! parented into a SoundCraft X11 window (see `ffi::UiInstance`). Main thread only.

use crate::ffi::UiInstance;
use crate::meta::nearest_index;
use crate::plugin::plugin_value;
use crate::scan::PortMap;
use livi::Features;
use soundcraft_dsp::{PluginEditor, PluginInfo};
use soundcraft_x11_window::X11Window;
use std::ffi::CString;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Window size before the UI asks for one or its size can be read.
const DEFAULT_SIDE: (f64, f64) = (400.0, 300.0);

/// A plugin's X11 UI in a SoundCraft window. `values` holds the current parameter values (f32
/// bits, parallel to `info.params`), written by the audio side; `features` is the plugin's feature
/// set of its last successful prepare.
pub(crate) struct Lv2Editor {
    info: &'static PluginInfo,
    map: &'static PortMap,
    values: Arc<[AtomicU32]>,
    features: Arc<Mutex<Option<Arc<Features>>>>,
    /// The plugin's LV2 URI (from its SoundCraft id).
    plugin_uri: CString,
    /// The latest value of each parameter this editor saw (edits in the UI, `set_param` from the
    /// host, also while closed), parallel to `info.params`. A reopened UI starts from these: the
    /// instance's `values` only change once the audio thread applies the session.
    known: Vec<Option<f32>>,
    open: Option<(X11Window, UiInstance)>,
}

impl Lv2Editor {
    pub(crate) fn new(
        info: &'static PluginInfo,
        map: &'static PortMap,
        values: Arc<[AtomicU32]>,
        features: Arc<Mutex<Option<Arc<Features>>>>,
        plugin_uri: CString,
    ) -> Lv2Editor {
        Lv2Editor { info, map, values, features, plugin_uri, known: vec![None; info.params.len()], open: None }
    }

    /// Notes `value` as parameter `id`'s latest (clamped; non-finite ignored). Its index, if known.
    fn remember(&mut self, id: &str, value: f32) -> Option<usize> {
        let i = self.info.params.iter().position(|p| p.id == id)?;
        if let (Some(p), Some(k)) = (self.info.params.get(i), self.known.get_mut(i))
            && value.is_finite()
        {
            *k = Some(p.clamp(value));
        }
        Some(i)
    }

    /// Closes the window: the UI is cleaned up first, then the window is destroyed.
    fn close_window(&mut self) {
        if let Some((win, ui)) = self.open.take() {
            drop(ui);
            win.close();
        }
    }
}

/// The parameter edits a batch of UI control writes stands for: `(param id, SoundCraft value)`.
/// Writes to ports that are not parameters are skipped, and so are non-finite values. An
/// enumeration's plugin value becomes the index of its nearest scale point. Only the latest value
/// per parameter is kept.
pub(crate) fn edits(info: &PluginInfo, map: &PortMap, writes: &[(u32, f32)]) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = Vec::new();
    for &(port, value) in writes {
        if !value.is_finite() {
            continue;
        }
        let Some(i) = map.params.iter().position(|pp| u32::try_from(pp.port).ok() == Some(port)) else { continue };
        let (Some(pp), Some(p)) = (map.params.get(i), info.params.get(i)) else { continue };
        let v = if pp.scale.is_empty() { value } else { nearest_index(&pp.scale, value) };
        let v = p.clamp(v);
        match out.iter_mut().find(|(id, _)| id == p.id) {
            Some(e) => e.1 = v,
            None => out.push((p.id.to_string(), v)),
        }
    }
    out
}

impl PluginEditor for Lv2Editor {
    fn open(&mut self) -> Result<(), String> {
        if let Some((win, _)) = &self.open {
            win.raise();
            return Ok(());
        }
        let features = self.features.lock().unwrap_or_else(PoisonError::into_inner).clone().ok_or("the plugin is not running yet")?;
        let ui = self.map.ui.as_ref().ok_or("this plugin has no X11 editor")?;
        let win = X11Window::new(self.info.name, DEFAULT_SIDE.0, DEFAULT_SIDE.1)?;
        let mut inst = match UiInstance::new(ui, &self.plugin_uri, win.id(), features) {
            Ok(inst) => inst,
            Err(e) => {
                win.close();
                return Err(e);
            }
        };
        for (i, pp) in self.map.params.iter().enumerate() {
            let (Some(bits), Ok(port)) = (self.values.get(i), u32::try_from(pp.port)) else { continue };
            let v = self.known.get(i).copied().flatten().unwrap_or_else(|| f32::from_bits(bits.load(Ordering::Relaxed)));
            inst.port_event(port, plugin_value(pp, v));
        }
        let (w, h) = match inst.take_resize() {
            Some((w, h)) => (f64::from(w), f64::from(h)),
            None => match win.child_size(inst.widget()) {
                Some((w, h)) => (f64::from(w), f64::from(h)),
                None => DEFAULT_SIDE,
            },
        };
        win.resize(w, h);
        self.open = Some((win, inst));
        Ok(())
    }

    fn close(&mut self) {
        self.close_window();
    }

    fn is_open(&self) -> bool {
        self.open.is_some()
    }

    fn idle(&mut self) -> Vec<(String, f32)> {
        let Some((win, ui)) = self.open.as_mut() else { return Vec::new() };
        if win.is_visible() && ui.idle() {
            if let Some((w, h)) = ui.take_resize() {
                win.resize(f64::from(w), f64::from(h));
            }
            let writes = ui.take_writes();
            let out = edits(self.info, self.map, &writes);
            for (id, v) in &out {
                self.remember(id, *v);
            }
            return out;
        }
        self.close_window();
        Vec::new()
    }

    fn set_param(&mut self, id: &str, value: f32) {
        let Some(i) = self.remember(id, value) else { return };
        let Some((_, ui)) = self.open.as_mut() else { return };
        let (Some(pp), Some(p)) = (self.map.params.get(i), self.info.params.get(i)) else { return };
        let Ok(port) = u32::try_from(pp.port) else { return };
        ui.port_event(port, plugin_value(pp, p.clamp(value)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ParamPort;
    use soundcraft_dsp::{ParamInfo, Taper, Unit};

    fn param(id: &'static str, min: f32, max: f32) -> ParamInfo {
        ParamInfo { id, name: id, min, max, default: min, unit: Unit::None, taper: Taper::Linear, choices: &[] }
    }

    /// Ports 5 (gain 0..1), 6 (mode: an enumeration with plugin values 0, 0.5, 1) and 9 (a
    /// 0..10 control). Port 7 is not a parameter.
    fn fixture() -> (&'static PluginInfo, &'static PortMap) {
        let params: &'static [ParamInfo] = Box::leak(Box::new([param("gain", 0.0, 1.0), param("mode", 0.0, 2.0), param("amount", 0.0, 10.0)]));
        let info: &'static PluginInfo = Box::leak(Box::new(PluginInfo {
            id: "lv2:urn:test",
            name: "Test",
            short_name: "Test",
            category: soundcraft_dsp::Category::Other,
            params,
            is_instrument: false,
            audiosuite: false,
            files: &[],
        }));
        let map = PortMap {
            params: vec![
                ParamPort { port: 5, scale: Vec::new() },
                ParamPort { port: 6, scale: vec![0.0, 0.5, 1.0] },
                ParamPort { port: 9, scale: Vec::new() },
            ],
            ..PortMap::default()
        };
        (info, Box::leak(Box::new(map)))
    }

    #[test]
    fn values_set_while_closed_are_what_a_reopened_ui_starts_from() {
        let (info, map) = fixture();
        let values: Arc<[AtomicU32]> = info.params.iter().map(|p| AtomicU32::new(p.default.to_bits())).collect::<Vec<_>>().into();
        let mut ed = Lv2Editor::new(info, map, values, Arc::new(Mutex::new(None)), CString::default());
        ed.set_param("gain", 0.5);
        ed.set_param("amount", 99.0);
        ed.set_param("mode", f32::NAN);
        ed.set_param("nope", 1.0);
        assert_eq!(ed.known, vec![Some(0.5), None, Some(10.0)], "clamped; NaN and unknown ids ignored");
    }

    #[test]
    fn writes_map_to_parameter_edits() {
        let (info, map) = fixture();
        let out = edits(info, map, &[(5, 0.25), (9, 42.0), (5, 0.75)]);
        assert_eq!(out, vec![("gain".to_string(), 0.75), ("amount".to_string(), 10.0)], "latest wins, out of range clamps");
    }

    #[test]
    fn an_enumeration_takes_the_index_of_its_nearest_scale_point() {
        let (info, map) = fixture();
        assert_eq!(edits(info, map, &[(6, 0.6)]), vec![("mode".to_string(), 1.0)]);
        assert_eq!(edits(info, map, &[(6, 0.9)]), vec![("mode".to_string(), 2.0)]);
    }

    #[test]
    fn unknown_ports_and_non_finite_values_are_skipped() {
        let (info, map) = fixture();
        assert!(edits(info, map, &[(7, 0.5), (99, 1.0)]).is_empty(), "ports that are not parameters");
        assert!(edits(info, map, &[(5, f32::NAN), (5, f32::INFINITY)]).is_empty());
        assert_eq!(edits(info, map, &[(5, f32::NAN), (5, 0.5)]), vec![("gain".to_string(), 0.5)]);
    }
}
