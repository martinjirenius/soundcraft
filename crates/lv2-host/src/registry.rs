//! The process-wide LV2 registry: the lilv world and scan cache, leaked `PluginInfo`s, the shared
//! feature sets, and instantiation.

use crate::meta;
use crate::plugin::Lv2Plugin;
use crate::scan::{self, PortMap};
use crate::{Lv2Descriptor, Lv2Error, parse_id};
use livi::{Features, FeaturesBuilder, World};
use soundcraft_dsp::{Plugin, PluginInfo};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

#[derive(Default)]
struct Registry {
    /// The lilv world, loaded by the first scan. Plugins are looked up in it; a rescan loads a fresh one.
    world: Option<World>,
    scanned: Option<Vec<Lv2Descriptor>>,
    /// One leaked `PluginInfo` (and its port map) per plugin, built on first use and kept.
    infos: HashMap<String, (&'static PluginInfo, &'static PortMap)>,
    /// The feature set of the last block length asked for. Instances keep their own `Arc`.
    features: Option<(usize, Arc<Features>)>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every audio effect or instrument in the world. Plugins with no audio output, or with CV ports
/// (not hosted yet), are left out.
fn scan_world(world: &World) -> Vec<Lv2Descriptor> {
    let mut out: Vec<Lv2Descriptor> = Vec::new();
    for plugin in world.iter_plugins() {
        let counts = *plugin.port_counts();
        if counts.audio_outputs == 0 || counts.cv_inputs + counts.cv_outputs > 0 {
            log::debug!("LV2 scan skipped {}: no audio output, or CV ports", plugin.uri());
            continue;
        }
        let uri = plugin.uri();
        if uri.is_empty() || uri == "BAD_URI" {
            continue;
        }
        let id = format!("{}{uri}", crate::ID_PREFIX);
        if out.iter().any(|d| d.id == id) {
            continue;
        }
        let classes: Vec<String> = plugin.classes().map(str::to_string).collect();
        let is_instrument = scan::is_instrument(&classes, counts.audio_inputs, counts.atom_sequence_inputs);
        let vendor = plugin.raw().author_name().and_then(|n| n.as_str().map(str::to_string)).unwrap_or_default();
        let path = plugin.raw().bundle_uri().as_uri().map(scan::bundle_path).unwrap_or_default();
        out.push(Lv2Descriptor {
            id,
            uri,
            name: plugin.name(),
            vendor,
            category: scan::category_from_classes(&classes, is_instrument),
            is_instrument,
            classes,
            path,
        });
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Scans on first use. Returns the cached list.
fn ensure_scanned(r: &mut Registry) -> &[Lv2Descriptor] {
    if r.scanned.is_none() {
        let world = World::new();
        let list = scan_world(&world);
        r.world = Some(world);
        r.scanned = Some(list);
    }
    r.scanned.as_deref().unwrap_or(&[])
}

pub fn scan() -> Vec<Lv2Descriptor> {
    ensure_scanned(&mut lock()).to_vec()
}

pub fn cached() -> Option<Vec<Lv2Descriptor>> {
    lock().scanned.clone()
}

pub fn rescan() -> Vec<Lv2Descriptor> {
    let mut r = lock();
    r.scanned = None;
    r.world = None;
    ensure_scanned(&mut r).to_vec()
}

/// Drops the lilv world, the scan cache and the feature sets. Instances already created keep
/// working (they hold what they need); the next scan loads the world again.
pub fn shutdown() {
    let mut r = lock();
    r.features = None;
    r.scanned = None;
    r.world = None;
}

/// The feature set for blocks of up to `max_block` frames, built once per block length.
pub(crate) fn features(max_block: usize) -> Arc<Features> {
    let mut guard = lock();
    let r = &mut *guard;
    if let Some((mb, f)) = &r.features
        && *mb == max_block
    {
        return Arc::clone(f);
    }
    let world = r.world.get_or_insert_with(World::new);
    let f = world.build_features(FeaturesBuilder { min_block_length: 1, max_block_length: max_block });
    r.features = Some((max_block, Arc::clone(&f)));
    f
}

/// The livi plugin for `id`, with its leaked info and port map (built on first use).
fn describe(id: &str) -> Result<(livi::Plugin, &'static PluginInfo, &'static PortMap), Lv2Error> {
    let uri = parse_id(id).ok_or_else(|| Lv2Error::NotFound(id.to_string()))?;
    let mut guard = lock();
    let r = &mut *guard;
    let desc = ensure_scanned(r).iter().find(|d| d.id == id).cloned().ok_or_else(|| Lv2Error::NotFound(id.to_string()))?;
    let Registry { world, infos, .. } = r;
    let world = world.as_ref().ok_or_else(|| Lv2Error::NotFound(id.to_string()))?;
    let plugin = world.plugin_by_uri(uri).ok_or_else(|| Lv2Error::NotFound(id.to_string()))?;
    if let Some(&(info, map)) = infos.get(id) {
        return Ok((plugin, info, map));
    }
    let (params, mut map) = meta::params(&plugin, world);
    let files = meta::files(&plugin, world, &mut map);
    let short: String = desc.name.chars().take(8).collect();
    let info: &'static PluginInfo = Box::leak(Box::new(PluginInfo {
        id: leak(desc.id.clone()),
        name: leak(desc.name.clone()),
        short_name: leak(short),
        category: desc.category,
        params: Box::leak(params.into_boxed_slice()),
        is_instrument: desc.is_instrument,
        audiosuite: false,
        files: Box::leak(files.into_boxed_slice()),
    }));
    map.ui = meta::x11_ui(&plugin, world);
    let map: &'static PortMap = Box::leak(Box::new(map));
    infos.insert(id.to_string(), (info, map));
    Ok((plugin, info, map))
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Creates a plugin by `lv2:<uri>`, prepared for 48 kHz stereo with 1024-frame blocks (like
/// `soundcraft_dsp::create`). A plugin whose instances could not be made reports `failed()`.
pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Lv2Error> {
    let (plugin, info, map) = describe(id)?;
    let mut p = Lv2Plugin::new(plugin, info, map);
    p.prepare(48_000.0, 1024, 2);
    Ok(Box::new(p))
}

pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    describe(id).ok().map(|(_, info, _)| info)
}
