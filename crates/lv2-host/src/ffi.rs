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

/// The X11 UI host: a plugin's own X11 editor (`ui:X11UI`) loaded from its UI binary and
/// instantiated into a SoundCraft X11 window. Only on the Linux/BSD targets that have X11.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
pub(crate) use x11ui::UiInstance;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
mod x11ui {
    //! The LV2 UI C ABI, hand-declared (the `lv2-sys` crate is not used). The UI runs on the main
    //! thread only; its callbacks receive the `UiHost` the instance owns.

    use crate::scan::UiInfo;
    use livi::Features;
    use std::collections::HashMap;
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    /// Most control writes a UI may queue between two `take_writes` calls.
    const MAX_WRITES: usize = 4096;
    /// Largest side (in pixels) a UI may ask for.
    const MAX_SIDE: c_int = 16_384;
    /// Most descriptors read from a UI binary.
    const MAX_DESCRIPTORS: u32 = 64;

    const URID_MAP: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
    const URID_UNMAP: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";
    const OPTIONS: &CStr = c"http://lv2plug.in/ns/ext/options#options";
    const UI_PARENT: &CStr = c"http://lv2plug.in/ns/extensions/ui#parent";
    const UI_RESIZE: &CStr = c"http://lv2plug.in/ns/extensions/ui#resize";
    const UI_IDLE: &CStr = c"http://lv2plug.in/ns/extensions/ui#idleInterface";

    type WriteFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void);
    type InstantiateFn = unsafe extern "C" fn(
        *const Descriptor,
        *const c_char,
        *const c_char,
        Option<WriteFn>,
        *mut c_void,
        *mut *mut c_void,
        *const *const Feature,
    ) -> *mut c_void;
    type CleanupFn = unsafe extern "C" fn(*mut c_void);
    type PortEventFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void);
    type ExtensionDataFn = unsafe extern "C" fn(*const c_char) -> *const c_void;
    type IdleFn = unsafe extern "C" fn(*mut c_void) -> c_int;
    type ResizeFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
    type DescriptorFn = unsafe extern "C" fn(u32) -> *const Descriptor;

    /// `LV2UI_Descriptor`.
    #[repr(C)]
    struct Descriptor {
        uri: *const c_char,
        instantiate: Option<InstantiateFn>,
        cleanup: Option<CleanupFn>,
        port_event: Option<PortEventFn>,
        extension_data: Option<ExtensionDataFn>,
    }

    /// `LV2_Feature`: the layout of livi's (`lv2_raw::LV2Feature`) and of the features this host adds.
    #[repr(C)]
    struct Feature {
        uri: *const c_char,
        data: *mut c_void,
    }

    /// `LV2UI_Resize`: the `ui#resize` feature's data.
    #[repr(C)]
    struct Resize {
        handle: *mut c_void,
        ui_resize: Option<ResizeFn>,
    }

    /// `LV2UI_Idle_Interface`: what `ui#idleInterface` returns from `extension_data`.
    #[repr(C)]
    struct IdleInterface {
        idle: Option<IdleFn>,
    }

    /// The state the UI's callbacks write into (their `controller` and resize `handle`).
    #[derive(Default)]
    struct UiHost {
        writes: Vec<(u32, f32)>,
        resize: Option<(u32, u32)>,
    }

    /// The UI's control-output callback. Accepts one float (protocol 0, 4 bytes) for a port;
    /// anything else is ignored, and so is a write past the queue's cap.
    unsafe extern "C" fn write_function(controller: *mut c_void, port: u32, size: u32, protocol: u32, buffer: *const c_void) {
        if protocol != 0 || size != 4 || buffer.is_null() {
            return;
        }
        // SAFETY: `controller` is the `UiHost` that `UiInstance::new` made, alive until the instance
        // is dropped (after the UI's cleanup), and used only on the main thread.
        let Some(host) = (unsafe { controller.cast::<UiHost>().as_mut() }) else { return };
        if host.writes.len() >= MAX_WRITES {
            return;
        }
        // SAFETY: the UI passes `size` (4) bytes holding a float; the buffer may be unaligned.
        let value = unsafe { buffer.cast::<f32>().read_unaligned() };
        host.writes.push((port, value));
    }

    /// The UI's `ui#resize` callback: a size of 1 to 16384 pixels per side is kept as the window's
    /// new size; anything else is refused (return 1).
    unsafe extern "C" fn ui_resize(handle: *mut c_void, width: c_int, height: c_int) -> c_int {
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return 1;
        }
        // SAFETY: `handle` is the `UiHost` of the instance that owns this `Resize`, as in `write_function`.
        let Some(host) = (unsafe { handle.cast::<UiHost>().as_mut() }) else { return 1 };
        host.resize = Some((width as u32, height as u32));
        0
    }

    /// Loaded UI binaries, kept for the life of the process: a UI library with static cairo or X
    /// state crashes when it is unloaded, so none is ever removed.
    fn libraries() -> &'static Mutex<HashMap<PathBuf, libloading::Library>> {
        static LIBS: OnceLock<Mutex<HashMap<PathBuf, libloading::Library>>> = OnceLock::new();
        LIBS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// The `lv2ui_descriptor` function of a UI binary, loading the binary on first use.
    fn descriptor_fn(binary: &Path) -> Result<DescriptorFn, String> {
        let mut libs = libraries().lock().unwrap_or_else(PoisonError::into_inner);
        if !libs.contains_key(binary) {
            // SAFETY: loading a plugin's UI binary runs the plugin's own initialisers, as creating a
            // plugin instance does (`ffi::instantiate`); it happens on the main thread.
            let lib = unsafe { libloading::Library::new(binary) }.map_err(|e| format!("cannot load the plugin's editor ({e})"))?;
            libs.insert(binary.to_path_buf(), lib);
        }
        let lib = libs.get(binary).ok_or("the plugin's editor is not loaded")?;
        // SAFETY: the symbol has the LV2 UI ABI that `DescriptorFn` declares. The library is never
        // removed from the cache, so the function and the descriptors it returns stay valid.
        let symbol = unsafe { lib.get::<DescriptorFn>(b"lv2ui_descriptor\0") }.map_err(|e| format!("the plugin's editor has no descriptor ({e})"))?;
        Ok(*symbol)
    }

    /// The descriptor of the UI binary whose URI is `uri`, if it has one.
    fn find_descriptor(descriptor: DescriptorFn, uri: &CStr) -> Option<*const Descriptor> {
        for i in 0..MAX_DESCRIPTORS {
            // SAFETY: calls the UI binary's descriptor function, which returns a static descriptor or null.
            let d = unsafe { descriptor(i) };
            if d.is_null() {
                return None;
            }
            // SAFETY: `d` is a non-null static descriptor in the loaded binary.
            let duri = unsafe { (*d).uri };
            // SAFETY: a descriptor's URI is a NUL-terminated string in the binary.
            if !duri.is_null() && unsafe { CStr::from_ptr(duri) } == uri {
                return Some(d);
            }
        }
        None
    }

    /// One instance of a plugin's X11 UI, parented into an X11 window. Dropping it cleans the UI up
    /// (the binary stays loaded).
    pub(crate) struct UiInstance {
        handle: *mut c_void,
        widget: *mut c_void,
        /// Static, in the UI binary (kept loaded). Never null.
        desc: *const Descriptor,
        idle: Option<IdleFn>,
        /// The `UiHost` and the `Resize` this instance gave the UI, from `Box::into_raw`.
        host: *mut UiHost,
        resize: *mut Resize,
        /// The feature array the UI was given: points into `own` and `_host_features`.
        features: Vec<*const Feature>,
        /// The features this host adds: `ui#parent`, `ui#resize` and `ui#idleInterface`.
        own: Box<[Feature]>,
        /// Keeps the plugin's own features (whose data the UI holds) alive for the instance's life.
        _host_features: Arc<Features>,
    }

    // SAFETY: an instance is used only on the main thread (as the CLAP editor's is). Its raw
    // pointers are owned by it or point into a UI library that stays loaded.
    unsafe impl Send for UiInstance {}

    impl UiInstance {
        /// Loads `ui`'s binary and instantiates the UI with `parent` (an X11 window id) as its parent
        /// window. `plugin_uri` is the plugin's LV2 URI; `host_features` supplies the URID map,
        /// unmap and options features the UI may use.
        pub(crate) fn new(ui: &UiInfo, plugin_uri: &CStr, parent: u32, host_features: Arc<Features>) -> Result<UiInstance, String> {
            let descriptor = descriptor_fn(&ui.binary)?;
            let desc = find_descriptor(descriptor, ui.uri.as_c_str()).ok_or("the plugin's editor has no matching UI")?;
            let host = Box::into_raw(Box::<UiHost>::default());
            let resize = Box::into_raw(Box::new(Resize { handle: host.cast(), ui_resize: Some(ui_resize) }));
            let own: Box<[Feature]> = vec![
                Feature { uri: UI_PARENT.as_ptr(), data: parent as usize as *mut c_void },
                Feature { uri: UI_RESIZE.as_ptr(), data: resize.cast() },
                Feature { uri: UI_IDLE.as_ptr(), data: ptr::null_mut() },
            ]
            .into_boxed_slice();
            // Dropping this value cleans nothing up (no handle yet) and frees `host` and `resize`.
            let mut inst = UiInstance {
                handle: ptr::null_mut(),
                widget: ptr::null_mut(),
                desc,
                idle: None,
                host,
                resize,
                features: Vec::new(),
                own,
                _host_features: host_features,
            };
            // The plugin's features that the UI may use, then this host's own; null-terminated.
            // livi's iterator also yields the worker feature it is given: pass a dummy and skip it by pointer.
            let none = lv2_raw::LV2Feature { uri: c"urn:soundcraft:none".as_ptr(), data: ptr::null_mut() };
            for f in inst._host_features.iter_features(&none) {
                if ptr::eq(f, &none) || f.uri.is_null() {
                    continue;
                }
                // SAFETY: the feature's URI is a NUL-terminated string that livi keeps alive for as long as the features.
                let uri = unsafe { CStr::from_ptr(f.uri) };
                if [URID_MAP, URID_UNMAP, OPTIONS].contains(&uri) {
                    // `lv2_raw::LV2Feature` is `#[repr(C)]` with the same two fields as `Feature`.
                    inst.features.push(ptr::from_ref(f).cast::<Feature>());
                }
            }
            for f in inst.own.iter() {
                inst.features.push(ptr::from_ref(f));
            }
            inst.features.push(ptr::null());
            let mut widget: *mut c_void = ptr::null_mut();
            // SAFETY: calls the UI's instantiate with the descriptor it returned, valid C strings, the
            // host callbacks, and a null-terminated feature array whose entries stay alive in `inst`.
            let handle = unsafe {
                let instantiate = (*desc).instantiate.ok_or("the plugin's editor cannot be created")?;
                instantiate(desc, plugin_uri.as_ptr(), ui.bundle.as_ptr(), Some(write_function), host.cast(), &mut widget, inst.features.as_ptr())
            };
            if handle.is_null() {
                return Err("the plugin's editor could not be created".to_string());
            }
            inst.handle = handle;
            inst.widget = widget;
            // SAFETY: the descriptor is valid (see above); extension data is looked up by URI.
            inst.idle = unsafe {
                let ext = (*desc).extension_data;
                let data = ext.map_or(ptr::null(), |f| f(UI_IDLE.as_ptr())).cast::<IdleInterface>();
                data.as_ref().and_then(|i| i.idle)
            };
            Ok(inst)
        }

        /// The UI's X11 child window (the one it created inside the parent), or 0 when it has none.
        pub(crate) fn widget(&self) -> u32 {
            self.widget as usize as u32
        }

        /// Tells the UI a port's value (`port_event`, format 0: a float).
        pub(crate) fn port_event(&mut self, port: u32, value: f32) {
            // SAFETY: the descriptor is static in the loaded binary and never null.
            if let Some(pe) = unsafe { (*self.desc).port_event } {
                // SAFETY: the UI's own handle; the buffer is a 4-byte float, as the size says.
                unsafe { pe(self.handle, port, 4, 0, ptr::from_ref(&value).cast()) };
            }
        }

        /// Runs the UI's idle callback. False when the UI asks to close (it returned nonzero).
        pub(crate) fn idle(&mut self) -> bool {
            match self.idle {
                // SAFETY: the idle function belongs to the UI this instance holds; the handle is its own.
                Some(idle) => unsafe { idle(self.handle) == 0 },
                None => true,
            }
        }

        /// The control writes the UI made since the last call, in order.
        pub(crate) fn take_writes(&mut self) -> Vec<(u32, f32)> {
            self.host_mut().map(|h| std::mem::take(&mut h.writes)).unwrap_or_default()
        }

        /// The latest size the UI asked for (`ui:resize`), if it asked since the last call.
        pub(crate) fn take_resize(&mut self) -> Option<(u32, u32)> {
            self.host_mut().and_then(|h| h.resize.take())
        }

        fn host_mut(&mut self) -> Option<&mut UiHost> {
            // SAFETY: `host` is the box made in `new`, alive until drop. The UI's callbacks run only
            // inside its own calls on this thread, so no other reference to the host is live here.
            unsafe { self.host.as_mut() }
        }
    }

    impl Drop for UiInstance {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                // SAFETY: the descriptor is valid; the handle came from its instantiate and is cleaned
                // up once, here, before the host state it points at is freed below.
                if let Some(cleanup) = unsafe { (*self.desc).cleanup } {
                    unsafe { cleanup(self.handle) };
                }
            }
            // SAFETY: `host` and `resize` came from `Box::into_raw` in `new` and are freed exactly once,
            // after the UI is gone.
            unsafe {
                drop(Box::from_raw(self.host));
                drop(Box::from_raw(self.resize));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn host() -> UiHost {
            UiHost::default()
        }

        fn write(h: &mut UiHost, port: u32, size: u32, protocol: u32, buffer: *const c_void) {
            // SAFETY: the test passes the host it owns, as the UI would.
            unsafe { write_function(ptr::from_mut(h).cast(), port, size, protocol, buffer) };
        }

        #[test]
        fn control_writes_keep_floats_of_the_right_shape_only() {
            let mut h = host();
            let v = 0.25f32;
            write(&mut h, 3, 4, 0, ptr::from_ref(&v).cast());
            assert_eq!(h.writes, vec![(3, 0.25)]);
            write(&mut h, 4, 8, 0, ptr::from_ref(&v).cast());
            write(&mut h, 4, 4, 1, ptr::from_ref(&v).cast());
            write(&mut h, 4, 4, 0, ptr::null());
            assert_eq!(h.writes.len(), 1, "wrong size, protocol or null buffer is ignored");
        }

        #[test]
        fn control_writes_are_capped() {
            let mut h = host();
            let v = 1.0f32;
            for _ in 0..MAX_WRITES + 10 {
                write(&mut h, 0, 4, 0, ptr::from_ref(&v).cast());
            }
            assert_eq!(h.writes.len(), MAX_WRITES);
        }

        #[test]
        fn a_null_controller_is_ignored() {
            let v = 1.0f32;
            // SAFETY: a null controller is what the callback must tolerate.
            unsafe { write_function(ptr::null_mut(), 0, 4, 0, ptr::from_ref(&v).cast()) };
        }

        #[test]
        fn resize_requests_are_bounded() {
            let mut h = host();
            let p = ptr::from_mut(&mut h).cast();
            // SAFETY: the test passes the host it owns, as the UI would.
            unsafe {
                assert_eq!(ui_resize(p, 0, 100), 1);
                assert_eq!(ui_resize(p, 100, -1), 1);
                assert_eq!(ui_resize(p, MAX_SIDE + 1, 100), 1);
                assert!(h.resize.is_none(), "refused sizes are not kept");
                assert_eq!(ui_resize(p, 640, MAX_SIDE), 0);
                assert_eq!(ui_resize(p, 1, 1), 0);
                assert_eq!(ui_resize(ptr::null_mut(), 640, 480), 1);
            }
            assert_eq!(h.resize, Some((1, 1)));
        }
    }
}
