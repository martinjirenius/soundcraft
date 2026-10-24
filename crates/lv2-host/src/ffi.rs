//! The only `unsafe` code in this crate: the two livi calls that run plugin code. Everything
//! here is wrapped in safe functions that return `Result` and never panic.

use livi::{Features, Instance, Plugin, PortConnections};
use std::sync::{Arc, Mutex, PoisonError};

/// The lock that serialises instantiating and freeing LV2 instances in this process (see
/// [`instantiate`] and [`free_instance`]).
static LIFECYCLE: Mutex<()> = Mutex::new(());

/// Creates an instance of `plugin` at `sample_rate`. The plugin's own code runs here (its
/// instantiate hook), so this is main-thread work.
pub(crate) fn instantiate(plugin: &Plugin, features: Arc<Features>, sample_rate: f64) -> Result<Instance, String> {
    let _lifecycle = LIFECYCLE.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: livi's contract for `Plugin::instantiate` is only that the caller accepts that the
    // plugin's code runs. The features outlive the instance (the instance holds its own Arc),
    // and the sample rate is finite and positive (checked by the caller).
    unsafe { plugin.instantiate(features, sample_rate) }.map_err(|e| format!("{e}"))
}

/// Drops instances (or a list of them) under the same lock as [`instantiate`]: lilv frees an
/// instance outside its world lock, and a plugin library that is instantiated while another
/// instance of it is freed crashes (observed with guitarix's plugins).
pub(crate) fn free_instances<T>(value: T) {
    let _lifecycle = LIFECYCLE.lock().unwrap_or_else(PoisonError::into_inner);
    drop(value);
}

/// Runs one block of `frames` samples. `ins` and `outs` are the plugin's audio port buffers, each
/// at least `frames` long; `atoms_in` / `atoms_out` are its atom sequence ports, in port order.
/// Returns false when the plugin reports a failure (the caller marks itself failed). Makes no
/// allocation: the port connections are built from the slices as iterators.
pub(crate) fn run_block(
    inst: &mut Instance,
    frames: usize,
    ins: &[Vec<f32>],
    outs: &mut [Vec<f32>],
    atoms_in: &[livi::event::LV2AtomSequence],
    atoms_out: &mut [livi::event::LV2AtomSequence],
) -> bool {
    let ports = PortConnections {
        audio_inputs: ins.iter().map(|v| v.get(..frames).unwrap_or_default()),
        audio_outputs: outs.iter_mut().map(|v| v.get_mut(..frames).unwrap_or_default()),
        atom_sequence_inputs: atoms_in.iter(),
        atom_sequence_outputs: atoms_out.iter_mut(),
        cv_inputs: std::iter::empty(),
        cv_outputs: std::iter::empty(),
    };
    // SAFETY: livi's contract for `Instance::run` is that the instance's code runs on this
    // thread with port buffers that stay alive for the call. The buffers are borrowed for the
    // call only, each is at least `frames` long, and the port counts match the instance (the
    // caller builds them from the same plugin), which livi checks and reports as an error.
    unsafe { inst.run(frames, ports) }.is_ok()
}
