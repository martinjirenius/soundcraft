//! A fixed-size top-level X11 window (`WM_CLASS` "soundcraft") that a plugin editor parents its
//! child window into. Each window owns its own connection, which `is_visible` drains for the
//! window manager's close request.

use std::cell::Cell;
use x11rb::COPY_FROM_PARENT;
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Largest side of a client area we ask for (the same bound as a plugin's size).
const MAX_SIDE: f64 = 16_384.0;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A client-area side in pixels: at least 1, at most `MAX_SIDE`; 640 for a non-finite request.
fn side(v: f64) -> u16 {
    if v.is_finite() { v.clamp(1.0, MAX_SIDE) as u16 } else { 640 }
}

fn atom(conn: &RustConnection, name: &[u8]) -> Result<Atom, String> {
    Ok(conn.intern_atom(false, name).map_err(err)?.reply().map_err(err)?.atom)
}

/// Owns the connection and the window; the window is destroyed in `close` (which drops the
/// connection). Nothing here blocks: events are drained without waiting.
pub struct X11Window {
    conn: RustConnection,
    win: Window,
    /// `WM_PROTOCOLS` and `WM_DELETE_WINDOW`, which name the window manager's close request.
    protocols: Atom,
    delete: Atom,
    /// The window manager asked to close the window or the connection failed: from then on
    /// `is_visible` is false.
    closed: Cell<bool>,
}

impl X11Window {
    pub fn new(title: &str, w: f64, h: f64) -> Result<X11Window, String> {
        let (conn, screen_num) = RustConnection::connect(None).map_err(|e| format!("no X11 display for plugin editors ({e})"))?;
        let screen = conn.setup().roots.get(screen_num).ok_or("the X11 display has no screen")?;
        let (root, visual, black) = (screen.root, screen.root_visual, screen.black_pixel);
        let (ww, wh) = (side(w), side(h));
        let protocols = atom(&conn, b"WM_PROTOCOLS")?;
        let delete = atom(&conn, b"WM_DELETE_WINDOW")?;
        let net_name = atom(&conn, b"_NET_WM_NAME")?;
        let utf8 = atom(&conn, b"UTF8_STRING")?;
        let win = conn.generate_id().map_err(err)?;
        let aux = CreateWindowAux::new().background_pixel(black).event_mask(EventMask::STRUCTURE_NOTIFY);
        conn.create_window(COPY_FROM_PARENT as u8, win, root, 0, 0, ww, wh, 0, WindowClass::INPUT_OUTPUT, visual, &aux).map_err(err)?;
        // Fixed size, like the Windows host window.
        let mut hints = WmSizeHints::new();
        hints.min_size = Some((i32::from(ww), i32::from(wh)));
        hints.max_size = Some((i32::from(ww), i32::from(wh)));
        hints.set_normal_hints(&conn, win).map_err(err)?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).map_err(err)?;
        conn.change_property8(PropMode::REPLACE, win, net_name, utf8, title.as_bytes()).map_err(err)?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"soundcraft\0soundcraft\0").map_err(err)?;
        conn.change_property32(PropMode::REPLACE, win, protocols, AtomEnum::ATOM, &[delete]).map_err(err)?;
        conn.map_window(win).map_err(err)?;
        conn.flush().map_err(err)?;
        Ok(X11Window { conn, win, protocols, delete, closed: Cell::new(false) })
    }

    /// The X11 window id a plugin editor creates its child window in (the child's parent).
    pub fn id(&self) -> u32 {
        self.win
    }

    pub fn raise(&self) {
        if self.closed.get() {
            return;
        }
        let _ = self.conn.map_window(self.win);
        let _ = self.conn.configure_window(self.win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = self.conn.flush();
    }

    /// Drains pending events without blocking. False once the window manager asked to close
    /// the window (`WM_DELETE_WINDOW`) or the connection failed, and for good after that.
    pub fn is_visible(&self) -> bool {
        if self.closed.get() {
            return false;
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::ClientMessage(ev))) => {
                    if ev.type_ == self.protocols && ev.data.as_data32().first() == Some(&self.delete) {
                        self.closed.set(true);
                        return false;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(_) => {
                    self.closed.set(true);
                    return false;
                }
            }
        }
    }

    /// Resizes the client area and keeps it fixed at the new size.
    pub fn resize(&self, w: f64, h: f64) {
        if self.closed.get() {
            return;
        }
        let (ww, wh) = (side(w), side(h));
        let mut hints = WmSizeHints::new();
        hints.min_size = Some((i32::from(ww), i32::from(wh)));
        hints.max_size = Some((i32::from(ww), i32::from(wh)));
        let _ = self.conn.configure_window(self.win, &ConfigureWindowAux::new().width(u32::from(ww)).height(u32::from(wh)));
        let _ = hints.set_normal_hints(&self.conn, self.win);
        let _ = self.conn.flush();
    }

    /// The size of a child window (its geometry), or None when it cannot be read (no such window,
    /// a closed connection, or no reply). One round trip to the server.
    pub fn child_size(&self, child: u32) -> Option<(u32, u32)> {
        if self.closed.get() || child == 0 {
            return None;
        }
        let reply = self.conn.get_geometry(child).ok()?.reply().ok()?;
        Some((u32::from(reply.width), u32::from(reply.height)))
    }

    /// Destroys the window and closes the connection.
    pub fn close(self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_clamps_and_defaults() {
        assert_eq!(side(0.0), 1);
        assert_eq!(side(1e9), 16_384);
        assert_eq!(side(f64::NAN), 640);
        assert_eq!(side(300.5), 300);
    }
}
