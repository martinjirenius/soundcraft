//! `Lv2Plugin`: an LV2 plugin behind the safe `soundcraft_dsp::Plugin` trait.
//!
//! Channel layout. LV2 port counts are fixed by the plugin, so a strip of `channels` maps onto
//! the instances as follows (plugin audio inputs A, outputs B):
//! - A = B = 1 on a strip wider than mono: one instance per strip channel, each processing that
//!   channel alone with the same control values.
//! - otherwise: one instance. Plugin input `j` is fed from strip channel `j` (silence when the
//!   strip has no such channel). Strip channel `c` takes plugin output `c` when there is one,
//!   and stays as it was (dry) when there is not. A plugin with no audio input (an instrument)
//!   gets no input; with fewer outputs than the strip has channels, the extra channels stay dry.
//!
//! Scratch buffers, atom sequences and the feature set are made in `prepare`; `process` makes no
//! allocation. Blocks longer than the prepared maximum are run in pieces.

use crate::ffi::{self, free_instances};
use crate::registry;
use crate::scan::{ParamPort, PortMap};
use livi::event::LV2AtomSequence;
use livi::{Features, Instance, PortIndex, WorkerManager};
use soundcraft_dsp::{Plugin, PluginInfo, PluginService, ServiceReport};
use std::sync::{Arc, Mutex, PoisonError};

/// Largest block we prepare for.
const MAX_BLOCK: usize = 1 << 16;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;
/// Bytes per atom sequence port (unit B puts events in them).
const ATOM_CAPACITY: usize = 8192;
/// Magic of the host's state blob (`SCL2`: LV2 host state, version 2).
const STATE_MAGIC: &[u8] = b"SCL2";
/// Largest state blob accepted (hostile input).
const MAX_STATE: usize = 1 << 20;

/// One LV2 instance with the buffers it runs on.
struct Lane {
    inst: Instance,
    /// Audio input buffers, one per plugin audio input.
    ins: Vec<Vec<f32>>,
    /// Audio output buffers, one per plugin audio output.
    outs: Vec<Vec<f32>>,
    atoms_in: Vec<LV2AtomSequence>,
    atoms_out: Vec<LV2AtomSequence>,
}

impl Lane {
    fn new(inst: Instance, shape: Shape, mb: usize, features: &Features) -> Lane {
        Lane {
            inst,
            ins: vec![vec![0.0; mb]; shape.audio_in],
            outs: vec![vec![0.0; mb]; shape.audio_out],
            atoms_in: (0..shape.atoms_in).map(|_| LV2AtomSequence::new(features, ATOM_CAPACITY)).collect(),
            atoms_out: (0..shape.atoms_out).map(|_| LV2AtomSequence::new(features, ATOM_CAPACITY)).collect(),
        }
    }

    /// Runs `frames` (at most the prepared block) on this lane. Atom inputs are emptied first, so
    /// events are per block. False when the plugin reports a failure.
    fn run(&mut self, frames: usize) -> bool {
        for seq in &mut self.atoms_in {
            seq.clear();
        }
        for seq in &mut self.atoms_out {
            seq.clear_as_chunk();
        }
        ffi::run_block(&mut self.inst, frames, &self.ins, &mut self.outs, &self.atoms_in, &mut self.atoms_out)
    }
}

/// The port counts of the plugin, as the channel layout needs them.
#[derive(Clone, Copy)]
struct Shape {
    audio_in: usize,
    audio_out: usize,
    atoms_in: usize,
    atoms_out: usize,
}

/// A hosted LV2 plugin. Parameter ids are the control ports' symbols; their names and ranges
/// are in `info().params`.
pub struct Lv2Plugin {
    plugin: livi::Plugin,
    info: &'static PluginInfo,
    map: &'static PortMap,
    shape: Shape,
    /// Current value of each parameter in SoundCraft units, parallel to `info.params`.
    values: Vec<f32>,
    lanes: Vec<Lane>,
    /// One instance per strip channel (A = B = 1 on a wider strip), else a single instance.
    per_channel: bool,
    channels: usize,
    max_block: usize,
    /// What the instances were prepared for (sample rate, block, channels). A repeat is a no-op.
    prepared: Option<(f64, usize, usize)>,
    /// Set when the instances could not be made or a block failed; `process` then leaves the
    /// buffers alone until a successful `prepare`.
    failed: bool,
    latency: usize,
    /// The worker manager of the current feature set, for the main-thread service.
    workers: Arc<Mutex<Option<Arc<WorkerManager>>>>,
}

impl Lv2Plugin {
    pub(crate) fn new(plugin: livi::Plugin, info: &'static PluginInfo, map: &'static PortMap) -> Lv2Plugin {
        let c = *plugin.port_counts();
        let shape =
            Shape { audio_in: c.audio_inputs, audio_out: c.audio_outputs, atoms_in: c.atom_sequence_inputs, atoms_out: c.atom_sequence_outputs };
        Lv2Plugin {
            plugin,
            info,
            map,
            shape,
            values: info.params.iter().map(|p| p.default).collect(),
            lanes: Vec::new(),
            per_channel: false,
            channels: 2,
            max_block: 0,
            prepared: None,
            failed: false,
            latency: 0,
            workers: Arc::new(Mutex::new(None)),
        }
    }

    fn param_index(&self, id: &str) -> Option<usize> {
        self.info.params.iter().position(|p| p.id == id)
    }
}

impl Drop for Lv2Plugin {
    fn drop(&mut self) {
        free_instances(std::mem::take(&mut self.lanes));
    }
}

/// The value the plugin's control port takes for a SoundCraft value (an enumeration index maps to
/// its scale point).
fn plugin_value(pp: &ParamPort, v: f32) -> f32 {
    if pp.scale.is_empty() {
        return v;
    }
    let index = v.round().max(0.0) as usize;
    pp.scale.get(index).copied().unwrap_or(v)
}

/// Writes every parameter, and the designated ports the host owns, to one instance.
fn apply_controls(lane: &mut Lane, map: &PortMap, values: &[f32]) {
    for (pp, v) in map.params.iter().zip(values) {
        let _ = lane.inst.set_control_input(PortIndex(pp.port), plugin_value(pp, *v));
    }
    if let Some(port) = map.enabled {
        let _ = lane.inst.set_control_input(PortIndex(port), 1.0);
    }
    if let Some(port) = map.freewheel {
        let _ = lane.inst.set_control_input(PortIndex(port), 0.0);
    }
}

/// The latency the plugin reports on its latency port, in samples (0 when it reports none).
fn latency_of(inst: &Instance, map: &PortMap) -> usize {
    map.latency.and_then(|port| inst.control_output(PortIndex(port))).filter(|v| v.is_finite() && *v > 0.0).map_or(0, |v| v.min(1.0e9) as usize)
}

impl Plugin for Lv2Plugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        let mb = max_block.clamp(1, MAX_BLOCK);
        let ch = channels.clamp(1, MAX_CHANNELS);
        let key = (sr, mb, ch);
        if self.prepared == Some(key) && !self.failed {
            return;
        }
        self.prepared = Some(key);
        self.latency = 0;
        self.failed = true;
        self.channels = ch;
        self.max_block = mb;
        self.per_channel = self.shape.audio_in == 1 && self.shape.audio_out == 1 && ch > 1;
        let features = registry::features(mb);
        free_instances(std::mem::take(&mut self.lanes));
        let count = if self.per_channel { ch } else { 1 };
        let mut lanes: Vec<Lane> = Vec::with_capacity(count);
        for _ in 0..count {
            match ffi::instantiate(&self.plugin, Arc::clone(&features), sr) {
                Ok(inst) => lanes.push(Lane::new(inst, self.shape, mb, &features)),
                Err(e) => {
                    log::warn!("{}: cannot instantiate: {e}", self.info.id);
                    return;
                }
            }
        }
        for lane in &mut lanes {
            apply_controls(lane, self.map, &self.values);
        }
        // Workers the plugin scheduled while instantiating, and one silent block so the latency
        // port has its value before the first real block.
        features.worker_manager().run_workers();
        if let Some(lane) = lanes.first_mut() {
            if !lane.run(mb) {
                log::warn!("{}: the first block failed", self.info.id);
                return;
            }
            self.latency = latency_of(&lane.inst, self.map);
        }
        *self.workers.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(features.worker_manager()));
        self.lanes = lanes;
        self.failed = false;
    }

    /// LV2 has no reset call and livi offers none, so the plugin keeps its internal state (a
    /// delay's or reverb's tail) until it is prepared again.
    fn reset(&mut self) {}

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.param_index(id) else { return false };
        let Some(pi) = self.info.params.get(i) else { return false };
        let v = pi.clamp(value);
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        if let Some(pp) = self.map.params.get(i) {
            for lane in &mut self.lanes {
                let _ = lane.inst.set_control_input(PortIndex(pp.port), plugin_value(pp, v));
            }
        }
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        self.values.get(self.param_index(id)?).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.failed || self.lanes.is_empty() {
            return;
        }
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if ch == 0 || frames == 0 {
            return;
        }
        let mut start = 0usize;
        let mut ok = true;
        while start < frames && ok {
            let n = (frames - start).min(self.max_block);
            let range = start..start + n;
            if self.per_channel {
                for (k, lane) in self.lanes.iter_mut().enumerate().take(ch) {
                    if let (Some(src), Some(dst)) = (io.get(k).and_then(|s| s.get(range.clone())), lane.ins.first_mut().and_then(|d| d.get_mut(..n)))
                    {
                        dst.copy_from_slice(src);
                    }
                    if !lane.run(n) {
                        ok = false;
                        break;
                    }
                    if let (Some(src), Some(dst)) = (lane.outs.first().and_then(|o| o.get(..n)), io.get_mut(k).and_then(|s| s.get_mut(range.clone())))
                    {
                        dst.copy_from_slice(src);
                    }
                }
            } else if let Some(lane) = self.lanes.first_mut() {
                for (j, input) in lane.ins.iter_mut().enumerate() {
                    let Some(dst) = input.get_mut(..n) else { continue };
                    match io.get(j).and_then(|s| s.get(range.clone())) {
                        Some(src) if j < ch => dst.copy_from_slice(src),
                        _ => dst.fill(0.0),
                    }
                }
                ok = lane.run(n);
                if ok {
                    for (c, output) in lane.outs.iter().enumerate().take(ch) {
                        if let (Some(src), Some(dst)) = (output.get(..n), io.get_mut(c).and_then(|s| s.get_mut(range.clone()))) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
            }
            start += n;
        }
        if !ok {
            // No logging on the audio path: the flag is the report.
            self.failed = true;
            return;
        }
        if let Some(lane) = self.lanes.first() {
            self.latency = latency_of(&lane.inst, self.map);
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn save_state(&mut self) -> Option<Vec<u8>> {
        let mut out = STATE_MAGIC.to_vec();
        for (p, v) in self.info.params.iter().zip(&self.values) {
            out.extend_from_slice(format!("{}={}\n", p.id, v).as_bytes());
        }
        Some(out)
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        if data.len() > MAX_STATE {
            return false;
        }
        let Some(body) = data.strip_prefix(STATE_MAGIC) else { return false };
        let Ok(text) = std::str::from_utf8(body) else { return false };
        for line in text.lines() {
            let Some((id, value)) = line.split_once('=') else { continue };
            let Ok(v) = value.trim().parse::<f32>() else { continue };
            if v.is_finite() {
                self.set_param(id.trim(), v);
            }
        }
        true
    }

    fn open_editor(&mut self) -> Result<(), String> {
        Err("LV2 editors are not hosted yet".to_string())
    }

    fn service_handle(&mut self) -> Option<Box<dyn PluginService>> {
        Some(Box::new(Lv2Service { workers: Arc::clone(&self.workers) }))
    }
}

/// The main-thread service point of a hosted LV2 plugin: runs the workers the plugin scheduled
/// (see [`PluginService`]).
struct Lv2Service {
    workers: Arc<Mutex<Option<Arc<WorkerManager>>>>,
}

impl PluginService for Lv2Service {
    fn service(&mut self) -> ServiceReport {
        let manager = self.workers.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(m) = manager {
            m.run_workers();
        }
        ServiceReport { values_changed: false, dirty: false }
    }
}
