//! zbus proxies for mutter's private screen-cast and remote-desktop interfaces.
//!
//! These are *not* the xdg-desktop-portal interfaces. They are mutter-internal and
//! carry no stability guarantee, which is exactly why every use of them is confined
//! to this crate -- an upstream rename should break one file, not the whole app.
//!
//! Signatures below are taken verbatim from mutter's published interface XML. Two
//! details are easy to get wrong and both have bitten this code already:
//!
//! * the `stream` argument of the `Notify*` methods is a **string**, not an object
//!   path, even though the value you pass is an object path;
//! * `NotifyTouchUp` takes only a slot -- no stream.

use std::collections::HashMap;

use zbus::proxy;
use zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Cursor rendering mode for a screen-cast stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CursorMode {
    /// Cursor is not captured at all.
    Hidden = 0,
    /// Cursor is composited into the video frames.
    Embedded = 1,
    /// Cursor is delivered out-of-band as PipeWire stream metadata.
    Metadata = 2,
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
pub trait ScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>)
        -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<i32>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
pub trait ScreenCastSession {
    /// Creates a monitor with no backing hardware. The monitor does not actually
    /// appear until PipeWire stream negotiation settles on a resolution.
    fn record_virtual(&self, properties: HashMap<&str, Value<'_>>)
        -> zbus::Result<OwnedObjectPath>;

    /// Captures an existing physical output, by connector name (e.g. `DP-3`).
    fn record_monitor(
        &self,
        connector: &str,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// Only valid for a session **not** linked to a remote-desktop session. When
    /// linked, mutter rejects this with "Must be started from remote desktop
    /// session" -- call [`RemoteDesktopSessionProxy::start`] instead.
    fn start(&self) -> zbus::Result<()>;

    fn stop(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
pub trait ScreenCastStream {
    /// Fires once negotiation completes and the PipeWire node exists. Subscribe
    /// *before* starting the session or the signal can be missed.
    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;

    /// Carries `position`/`size` in compositor coordinates once known, plus a
    /// `mapping-id` used to correlate the stream with its monitor.
    #[zbus(property)]
    fn parameters(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop",
    default_service = "org.gnome.Mutter.RemoteDesktop",
    default_path = "/org/gnome/Mutter/RemoteDesktop"
)]
pub trait RemoteDesktop {
    fn create_session(&self) -> zbus::Result<OwnedObjectPath>;

    /// Bitmask: 1 = keyboard, 2 = pointer, 4 = touchscreen.
    #[zbus(property)]
    fn supported_device_types(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<i32>;
}

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop.Session",
    default_service = "org.gnome.Mutter.RemoteDesktop"
)]
pub trait RemoteDesktopSession {
    /// Starts this session *and* any screen-cast session linked to it via
    /// `remote-desktop-session-id`.
    fn start(&self) -> zbus::Result<()>;

    /// Symmetric with [`start`](Self::start): a linked screen-cast session must
    /// also be stopped from this side.
    fn stop(&self) -> zbus::Result<()>;

    /// The value to pass as `remote-desktop-session-id` when creating the
    /// screen-cast session.
    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<String>;

    // --- input injection -------------------------------------------------
    // `stream` is the stream's object path *as a string*; coordinates are
    // relative to that stream, so they land on the virtual monitor with no
    // geometry maths on our side.

    fn notify_touch_down(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_touch_motion(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_touch_up(&self, slot: u32) -> zbus::Result<()>;

    fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_pointer_button(&self, button: i32, state: bool) -> zbus::Result<()>;
    fn notify_pointer_axis(&self, dx: f64, dy: f64, flags: u32) -> zbus::Result<()>;

    fn notify_keyboard_keycode(&self, keycode: u32, state: bool) -> zbus::Result<()>;
    fn notify_keyboard_keysym(&self, keysym: u32, state: bool) -> zbus::Result<()>;
}
