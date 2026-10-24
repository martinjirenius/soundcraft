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

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
use crate::editor::Lv2Editor;
use crate::ffi::{self, free_instances};
use crate::registry;
use crate::scan::{ParamPort, PortMap};
use livi::event::{LV2AtomEventBuilder, LV2AtomSequence};
use livi::{Features, Instance, PortIndex};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, PluginService, ServiceReport};
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
use std::ffi::CString;
use std::sync::atomic::{AtomicU32, Ordering};
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
/// Longest file path a file parameter takes (`set_file` refuses longer ones).
pub const MAX_FILE_PATH: usize = 4096;
/// Largest `patch:Set` event body: the object header, two properties and a path with its NUL,
/// padded (see [`write_set_body`]).
const EVENT_BYTES: usize = 4160;
/// Room one `patch:Set` event takes in its atom sequence: its header and body.
const EVENT_SPACE: usize = EVENT_BYTES + 32;

/// URIDs of the atom and patch terms a file parameter is delivered with. Zero when unmapped.
#[derive(Clone, Copy, Default)]
struct PatchUrids {
    /// `atom:Object`, the event's type.
    object: u32,
    /// `patch:Set`, the object's type.
    set: u32,
    /// `patch:property` and `patch:value`, the keys of the two properties.
    property: u32,
    value: u32,
    /// `atom:URID` and `atom:Path`, the value types.
    urid_type: u32,
    path_type: u32,
}

impl PatchUrids {
    fn new(features: &Features) -> PatchUrids {
        let urid = |uri: &[u8]| std::ffi::CStr::from_bytes_with_nul(uri).map_or(0, |c| features.urid(c));
        PatchUrids {
            object: urid(b"http://lv2plug.in/ns/ext/atom#Object\0"),
            set: urid(b"http://lv2plug.in/ns/ext/patch#Set\0"),
            property: urid(b"http://lv2plug.in/ns/ext/patch#property\0"),
            value: urid(b"http://lv2plug.in/ns/ext/patch#value\0"),
            urid_type: urid(b"http://lv2plug.in/ns/ext/atom#URID\0"),
            path_type: urid(b"http://lv2plug.in/ns/ext/atom#Path\0"),
        }
    }
}

/// One file parameter as the plugin holds it. The path and the body built from it are kept
/// across `prepare`, so new instances get the file again.
struct FileSlot {
    /// URID of the property (the parameter's id).
    prop: u32,
    /// The path last set (at most `MAX_FILE_PATH` bytes, reserved in `with_capacity`).
    path: Vec<u8>,
    /// The `patch:Set` object body for `path` (reserved in `with_capacity`).
    body: Vec<u8>,
    /// Not yet delivered to the instances.
    pending: bool,
}

impl FileSlot {
    fn with_capacity() -> FileSlot {
        FileSlot { prop: 0, path: Vec::with_capacity(MAX_FILE_PATH), body: Vec::with_capacity(EVENT_BYTES), pending: false }
    }
}

/// Where pending file parameters go: the atom input port, the event type, and the slots.
#[derive(Clone, Copy)]
struct Delivery<'a> {
    port: usize,
    event_type: u32,
    files: &'a [FileSlot],
}

/// Writes the object body of a `patch:Set` that sets property `prop` to `path`: the object's id
/// and type, then `patch:property` (an `atom:URID` naming `prop`) and `patch:value` (an
/// `atom:Path`, NUL-terminated). Every atom is padded to 8 bytes, relative to the body's start,
/// which is 8-aligned in the sequence. The event that carries it is typed `atom:Object`.
/// Returns false, leaving `out` empty, when the path is too long or `out` cannot hold the body
/// without growing (the caller reserves it, so this never allocates).
fn write_set_body(out: &mut Vec<u8>, u: &PatchUrids, prop: u32, path: &[u8]) -> bool {
    out.clear();
    if path.len() > MAX_FILE_PATH || path.contains(&0) {
        return false;
    }
    let value_len = path.len() + 1;
    let total = 48 + pad8(value_len);
    if out.capacity() < total {
        return false;
    }
    put_u32(out, 0);
    put_u32(out, u.set);
    put_u32(out, u.property);
    put_u32(out, 0);
    put_u32(out, 4);
    put_u32(out, u.urid_type);
    put_u32(out, prop);
    pad8_vec(out);
    put_u32(out, u.value);
    put_u32(out, 0);
    put_u32(out, value_len as u32);
    put_u32(out, u.path_type);
    out.extend_from_slice(path);
    out.push(0);
    pad8_vec(out);
    out.len() == total
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_ne_bytes());
}

/// Bytes of `n` rounded up to a multiple of 8.
fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

fn pad8_vec(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }
}

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
    /// `patch` is the atom input that takes file parameters, with room for `files` of them.
    fn new(inst: Instance, shape: Shape, mb: usize, features: &Features, patch: Option<(usize, usize)>) -> Lane {
        let in_capacity = |i: usize| match patch {
            Some((port, files)) if port == i => ATOM_CAPACITY + files * EVENT_SPACE,
            _ => ATOM_CAPACITY,
        };
        Lane {
            inst,
            ins: vec![vec![0.0; mb]; shape.audio_in],
            outs: vec![vec![0.0; mb]; shape.audio_out],
            atoms_in: (0..shape.atoms_in).map(|i| LV2AtomSequence::new(features, in_capacity(i))).collect(),
            atoms_out: (0..shape.atoms_out).map(|_| LV2AtomSequence::new(features, ATOM_CAPACITY)).collect(),
        }
    }

    /// Runs `frames` (at most the prepared block) on this lane. Atom inputs are emptied first, so
    /// events are per block; pending file parameters are put into the patch port first. False
    /// when the plugin reports a failure, or a file event does not fit.
    fn run(&mut self, frames: usize, delivery: Option<Delivery<'_>>) -> bool {
        for seq in &mut self.atoms_in {
            seq.clear();
        }
        for seq in &mut self.atoms_out {
            seq.clear_as_chunk();
        }
        if let Some(d) = delivery {
            let Some(seq) = self.atoms_in.get_mut(d.port) else { return false };
            for f in d.files.iter().filter(|f| f.pending) {
                let Ok(event) = LV2AtomEventBuilder::<EVENT_BYTES>::new(0, d.event_type, &f.body) else { return false };
                if seq.push_event(&event).is_err() {
                    return false;
                }
            }
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
    /// The feature set of the last successful prepare (its worker manager runs in the service, and
    /// its URID map and options go to a UI). Empty until the first prepare succeeds.
    features: Arc<Mutex<Option<Arc<Features>>>>,
    /// The current parameter values as f32 bits, parallel to `info.params`, for the editor's
    /// reads (written with `Relaxed` atomics by `set_param`, never locked).
    values_shared: Arc<[AtomicU32]>,
    /// URIDs of the current feature set (zero before the first `prepare`).
    patch: PatchUrids,
    /// One slot per `info.files` entry, in order.
    files: Vec<FileSlot>,
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
            features: Arc::new(Mutex::new(None)),
            values_shared: info.params.iter().map(|p| AtomicU32::new(p.default.to_bits())).collect::<Vec<_>>().into(),
            patch: PatchUrids::default(),
            files: Vec::new(),
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
pub(crate) fn plugin_value(pp: &ParamPort, v: f32) -> f32 {
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
        self.patch = PatchUrids::new(&features);
        // File parameters survive a prepare (a new instance needs its model again): every slot
        // that holds a path is delivered once more.
        let mut files = std::mem::take(&mut self.files);
        files.resize_with(self.info.files.len(), FileSlot::with_capacity);
        for (slot, info) in files.iter_mut().zip(self.info.files) {
            slot.prop = std::ffi::CString::new(info.id).map_or(0, |c| features.urid(c.as_c_str()));
            slot.pending = slot.prop != 0 && !slot.path.is_empty() && write_set_body(&mut slot.body, &self.patch, slot.prop, &slot.path);
        }
        self.files = files;
        let patch = self.map.patch_input.map(|port| (port, self.info.files.len()));
        let count = if self.per_channel { ch } else { 1 };
        let mut lanes: Vec<Lane> = Vec::with_capacity(count);
        for _ in 0..count {
            match ffi::instantiate(&self.plugin, Arc::clone(&features), sr) {
                Ok(inst) => lanes.push(Lane::new(inst, self.shape, mb, &features, patch)),
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
            if !lane.run(mb, None) {
                log::warn!("{}: the first block failed", self.info.id);
                return;
            }
            self.latency = latency_of(&lane.inst, self.map);
        }
        *self.features.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&features));
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
        if let Some(shared) = self.values_shared.get(i) {
            shared.store(v.to_bits(), Ordering::Relaxed);
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

    /// Queues `path` for file parameter `id`: it is put into the instances' patch port at the
    /// start of the next block. Builds the event body into the slot's reserved buffer (no
    /// allocation); refuses an unknown id, a path too long or empty (or with a NUL), and a
    /// plugin not yet prepared or without a patch input.
    fn set_file(&mut self, id: &str, path: &str) -> bool {
        let Some(i) = self.info.files.iter().position(|f| f.id == id) else { return false };
        if self.map.patch_input.is_none() || self.patch.object == 0 || path.is_empty() || path.len() > MAX_FILE_PATH {
            return false;
        }
        let Some(slot) = self.files.get_mut(i) else { return false };
        if slot.prop == 0 || !write_set_body(&mut slot.body, &self.patch, slot.prop, path.as_bytes()) {
            return false;
        }
        slot.path.clear();
        slot.path.extend_from_slice(path.as_bytes());
        slot.pending = true;
        true
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
        // Pending file parameters go to every instance in the block's first piece.
        let deliver = self.map.patch_input.filter(|_| self.patch.object != 0 && self.files.iter().any(|f| f.pending));
        let mut start = 0usize;
        let mut ok = true;
        while start < frames && ok {
            let n = (frames - start).min(self.max_block);
            let range = start..start + n;
            let delivery = match deliver {
                Some(port) if start == 0 => Some(Delivery { port, event_type: self.patch.object, files: &self.files }),
                _ => None,
            };
            if self.per_channel {
                for (k, lane) in self.lanes.iter_mut().enumerate().take(ch) {
                    if let (Some(src), Some(dst)) = (io.get(k).and_then(|s| s.get(range.clone())), lane.ins.first_mut().and_then(|d| d.get_mut(..n)))
                    {
                        dst.copy_from_slice(src);
                    }
                    if !lane.run(n, delivery) {
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
                ok = lane.run(n, delivery);
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
        if deliver.is_some() {
            for f in &mut self.files {
                f.pending = false;
            }
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

    /// The plugin's X11 UI as an editor (a SoundCraft X11 window). None when the plugin has no X11
    /// UI, or on a platform without X11.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        self.map.ui.as_ref()?;
        let plugin_uri = CString::new(crate::parse_id(self.info.id)?).ok()?;
        Some(Box::new(Lv2Editor::new(self.info, self.map, Arc::clone(&self.values_shared), Arc::clone(&self.features), plugin_uri)))
    }

    #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "android"))))]
    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        None
    }

    fn service_handle(&mut self) -> Option<Box<dyn PluginService>> {
        Some(Box::new(Lv2Service { features: Arc::clone(&self.features) }))
    }
}

/// The main-thread service point of a hosted LV2 plugin: runs the workers the plugin scheduled
/// (see [`PluginService`]).
struct Lv2Service {
    features: Arc<Mutex<Option<Arc<Features>>>>,
}

impl PluginService for Lv2Service {
    fn service(&mut self) -> ServiceReport {
        let features = self.features.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(f) = features {
            f.worker_manager().run_workers();
        }
        ServiceReport { values_changed: false, dirty: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn set_body_has_the_patch_set_layout_with_padding() {
        let u = PatchUrids { object: 1, set: 2, property: 3, value: 4, urid_type: 5, path_type: 6 };
        let mut out = Vec::with_capacity(EVENT_BYTES);
        let path = b"/m/a.nam";
        assert!(write_set_body(&mut out, &u, 7, path));
        // Object: id 0, type patch:Set (48 bytes for an 8-byte path + NUL, padded to 16).
        assert_eq!(out.len(), 48 + 16);
        assert_eq!((word(&out, 0), word(&out, 4)), (0, 2));
        // patch:property = URID of the parameter, inside an atom of 4 bytes padded to 16.
        assert_eq!((word(&out, 8), word(&out, 12)), (3, 0));
        assert_eq!((word(&out, 16), word(&out, 20), word(&out, 24)), (4, 5, 7));
        assert_eq!(word(&out, 28), 0, "padding after the 4-byte URID");
        // patch:value = the path with its NUL, an atom of len+1 bytes padded to 8.
        assert_eq!((word(&out, 32), word(&out, 36)), (4, 0));
        assert_eq!((word(&out, 40), word(&out, 44)), (9, 6));
        assert_eq!(&out[48..56], path);
        assert_eq!(out[56], 0, "NUL terminator");
        assert!(out[57..].iter().all(|&b| b == 0), "zero padding");
        // Every atom starts 8-aligned relative to the body.
        for at in [16usize, 32, 48] {
            assert_eq!(at % 8, 0);
        }
    }

    #[test]
    fn set_body_refuses_long_paths_nul_and_small_buffers() {
        let u = PatchUrids { object: 1, set: 2, property: 3, value: 4, urid_type: 5, path_type: 6 };
        let mut out = Vec::with_capacity(EVENT_BYTES);
        let long = vec![b'a'; MAX_FILE_PATH + 1];
        assert!(!write_set_body(&mut out, &u, 7, &long));
        assert!(!write_set_body(&mut out, &u, 7, b"/a\0b"));
        let mut small = Vec::with_capacity(16);
        assert!(!write_set_body(&mut small, &u, 7, b"/m/a.nam"));
        assert!(small.is_empty());
        // The longest allowed path still fits the reserved event size.
        let max = vec![b'a'; MAX_FILE_PATH];
        assert!(write_set_body(&mut out, &u, 7, &max));
        assert!(out.len() <= EVENT_BYTES);
    }
}
