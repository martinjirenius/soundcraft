//! `ParkGate`: how the main thread takes a plugin away from the audio thread for a moment
//! (deactivate and re-activate it, flush it) without the audio thread ever locking or waiting.
//!
//! The audio thread brackets each call into the plugin with [`ParkGate::enter`]. While the main
//! thread holds a [`Parked`] token, `enter` answers `None` and the audio thread treats the
//! plugin as inactive for that block (a hosted plugin then leaves its buffers untouched: an
//! effect passes its input through, an instrument stays silent). The main thread waits only for a
//! block that is already running, and gives up after a bound (the caller retries on its next
//! service tick) rather than touching a plugin the audio thread is inside.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A two-flag handshake (Dekker style, sequentially consistent) between one audio thread and one
/// main thread.
#[derive(Debug, Default)]
pub struct ParkGate {
    park: AtomicBool,
    busy: AtomicBool,
}

/// Held by the audio thread while it is inside the plugin.
pub struct Busy<'a>(&'a ParkGate);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

/// Held by the main thread while the audio thread is kept out of the plugin.
pub struct Parked<'a>(&'a ParkGate);

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.0.park.store(false, Ordering::SeqCst);
    }
}

impl ParkGate {
    /// Audio thread: `Some` when it may call into the plugin (until the token drops); `None`
    /// while the main thread has the plugin parked. Never blocks, locks or allocates.
    pub fn enter(&self) -> Option<Busy<'_>> {
        if self.park.load(Ordering::SeqCst) {
            return None;
        }
        self.busy.store(true, Ordering::SeqCst);
        if self.park.load(Ordering::SeqCst) {
            self.busy.store(false, Ordering::SeqCst);
            return None;
        }
        Some(Busy(self))
    }

    /// Main thread: keeps the audio thread out of the plugin. Waits at most `limit` for a call
    /// the audio thread is already inside; `None` when it did not finish in time (nothing was
    /// touched; try again later).
    pub fn park(&self, limit: Duration) -> Option<Parked<'_>> {
        self.park.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while self.busy.load(Ordering::SeqCst) {
            if start.elapsed() > limit {
                self.park.store(false, Ordering::SeqCst);
                return None;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        Some(Parked(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parking_excludes_the_audio_thread_and_gives_up_on_a_stuck_one() {
        let g = ParkGate::default();
        {
            let p = g.park(Duration::from_millis(10)).expect("idle gate parks");
            assert!(g.enter().is_none(), "audio thread kept out while parked");
            drop(p);
        }
        let busy = g.enter().expect("free again");
        assert!(g.park(Duration::from_millis(5)).is_none(), "a running call is never interrupted");
        assert!(g.enter().is_some(), "a failed park leaves the gate open");
        drop(busy);
    }
}
