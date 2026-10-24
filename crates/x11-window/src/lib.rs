//! SoundCraft X11 host window: a fixed-size top-level X11 window that a plugin's editor parents its
//! own child window into. Used by the CLAP host and, later, the LV2 and VST3 hosts.
//!
//! The window talks to the X server over x11rb's pure-Rust connection. On targets without X11
//! (macOS, Android, Windows, wasm) the crate is empty.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
mod x11;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
pub use x11::X11Window;
