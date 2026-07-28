//! Creating and driving a mutter virtual monitor.
//!
//! The choreography this crate implements was established empirically against
//! GNOME Shell 50.3 / mutter 50.3, and the ordering is not obvious from the
//! interface XML alone:
//!
//! 1. `RemoteDesktop.CreateSession` -> read its `SessionId`.
//! 2. `ScreenCast.CreateSession` passing `remote-desktop-session-id`. Linking the
//!    two is what makes injected input coordinates land on the virtual monitor.
//! 3. `ScreenCast.Session.RecordVirtual` with the desired modes.
//! 4. Subscribe to `PipeWireStreamAdded` **before** starting.
//! 5. `RemoteDesktop.Session.Start` -- *not* `ScreenCast.Session.Start`, which
//!    mutter rejects for a linked session.
//! 6. Teardown is symmetric: `RemoteDesktop.Session.Stop`.
//!
//! The D-Bus connection must outlive the session: mutter reaps sessions when the
//! owning peer disconnects, so [`Session`] holds its `Connection`.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use tracing::{debug, info, warn};
use zbus::Connection;
use zvariant::{OwnedObjectPath, Value};

pub mod keys;
mod proxies;

pub use keys::Chord;
pub use proxies::CursorMode;
use proxies::{
    RemoteDesktopProxy, RemoteDesktopSessionProxy, ScreenCastProxy, ScreenCastSessionProxy,
    ScreenCastStreamProxy,
};

/// How long to wait for PipeWire to negotiate the stream before giving up.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),

    #[error("this needs a GNOME/mutter session on the session bus -- is GNOME running?")]
    NoMutter,

    #[error(
        "mutter's ScreenCast API is version {found}, but virtual monitors with pinned modes \
         need version {required} (GNOME 46+)"
    )]
    ApiTooOld { found: i32, required: i32 },

    #[error("PipeWire stream negotiation timed out after {}s -- mutter never emitted PipeWireStreamAdded", NEGOTIATION_TIMEOUT.as_secs())]
    NegotiationTimeout,

    #[error("mutter reports no touchscreen support (SupportedDeviceTypes = {0:#b})")]
    NoTouchSupport(u32),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Bit 2 of `RemoteDesktop.SupportedDeviceTypes`.
const DEVICE_TYPE_TOUCHSCREEN: u32 = 4;

/// `RecordVirtual` with pinned `modes` requires ScreenCast API v3+; we saw v4.
const REQUIRED_SCREENCAST_VERSION: i32 = 3;

/// What the tablet should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// A new monitor with no backing hardware -- the desktop is *extended*.
    Virtual,
    /// An existing physical output, by connector name (e.g. `DP-3`) -- *mirrored*.
    Monitor(String),
}

#[derive(Debug, Clone)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: f64,
    pub cursor_mode: CursorMode,
    pub source: CaptureSource,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            refresh_rate: 60.0,
            cursor_mode: CursorMode::Embedded,
            source: CaptureSource::Virtual,
        }
    }
}

/// A live virtual monitor plus its input channel.
///
/// Dropping this stops the session and removes the monitor.
pub struct Session {
    // Held solely to keep the peer connected; mutter tears the session down if the
    // owning D-Bus connection goes away.
    _conn: Connection,
    remote_desktop: RemoteDesktopSessionProxy<'static>,
    screen_cast: ScreenCastSessionProxy<'static>,
    /// Object path of the stream, as a string -- the form the `Notify*` methods want.
    stream_path: String,
    /// PipeWire node to consume with `pipewiresrc path=<node_id>`.
    node_id: u32,
    config: DisplayConfig,
    // Atomic rather than a bool so the session can be shared behind an `Arc` --
    // the touch task and the teardown path both need it, and neither can take
    // ownership.
    stopped: std::sync::atomic::AtomicBool,
}

impl Session {
    /// Runs the full setup and returns once the PipeWire node exists.
    pub async fn open(config: DisplayConfig) -> Result<Self> {
        let conn = Connection::session().await?;

        let remote_desktop = RemoteDesktopProxy::new(&conn)
            .await
            .map_err(|_| Error::NoMutter)?;
        let screen_cast = ScreenCastProxy::new(&conn)
            .await
            .map_err(|_| Error::NoMutter)?;

        let version = screen_cast.version().await?;
        if version < REQUIRED_SCREENCAST_VERSION {
            return Err(Error::ApiTooOld {
                found: version,
                required: REQUIRED_SCREENCAST_VERSION,
            });
        }
        let devices = remote_desktop.supported_device_types().await?;
        if devices & DEVICE_TYPE_TOUCHSCREEN == 0 {
            // Not fatal for display-only use, but touch is a headline feature.
            warn!(
                devices,
                "mutter reports no touchscreen support; touch will not work"
            );
        }
        debug!(
            screencast_version = version,
            device_types = devices,
            "mutter APIs ready"
        );

        // 1. Remote-desktop session, whose id links the screen-cast session to it.
        let rd_path: OwnedObjectPath = remote_desktop.create_session().await?;
        let rd_session = RemoteDesktopSessionProxy::new(&conn, rd_path.clone()).await?;
        let rd_id = rd_session.session_id().await?;

        // 2. Screen-cast session, linked.
        let mut sc_props: HashMap<&str, Value<'_>> = HashMap::new();
        sc_props.insert("remote-desktop-session-id", Value::from(rd_id.as_str()));
        let sc_path = screen_cast.create_session(sc_props).await?;
        let sc_session = ScreenCastSessionProxy::new(&conn, sc_path.clone()).await?;

        // 3. Create the stream -- virtual monitor, or a mirror of a real one.
        let stream_path = match &config.source {
            CaptureSource::Virtual => {
                let mode: HashMap<&str, Value<'_>> = HashMap::from([
                    ("size", Value::from((config.width, config.height))),
                    ("refresh-rate", Value::from(config.refresh_rate)),
                    ("is-preferred", Value::from(true)),
                ]);
                let props: HashMap<&str, Value<'_>> = HashMap::from([
                    // Behave as a real platform monitor: shows up in Settings ->
                    // Displays, can be arranged, holds workspaces.
                    ("is-platform", Value::from(true)),
                    ("cursor-mode", Value::from(config.cursor_mode as u32)),
                    // Pinning modes stops PipeWire renegotiating us to some other
                    // resolution. Requires GNOME 50+; harmless to send to older
                    // mutter, which ignores unknown keys.
                    ("modes", Value::from(vec![mode])),
                ]);
                sc_session.record_virtual(props).await?
            }
            CaptureSource::Monitor(connector) => {
                let props: HashMap<&str, Value<'_>> =
                    HashMap::from([("cursor-mode", Value::from(config.cursor_mode as u32))]);
                sc_session.record_monitor(connector, props).await?
            }
        };

        // 4. Subscribe before starting, or the signal can arrive before we listen.
        let stream = ScreenCastStreamProxy::new(&conn, stream_path.clone()).await?;
        let mut added = stream.receive_pipe_wire_stream_added().await?;

        // 5. Start from the remote-desktop side; the linked screen-cast starts with it.
        rd_session.start().await?;

        let node_id = match tokio::time::timeout(NEGOTIATION_TIMEOUT, added.next()).await {
            Ok(Some(signal)) => signal.args()?.node_id,
            Ok(None) | Err(_) => {
                let _ = rd_session.stop().await;
                return Err(Error::NegotiationTimeout);
            }
        };

        info!(
            node_id,
            width = config.width,
            height = config.height,
            source = ?config.source,
            "virtual monitor is live"
        );

        Ok(Self {
            _conn: conn,
            remote_desktop: rd_session,
            screen_cast: sc_session,
            stream_path: stream_path.as_str().to_owned(),
            node_id,
            config,
            stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// PipeWire node id to feed to `pipewiresrc path=...`.
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn config(&self) -> &DisplayConfig {
        &self.config
    }

    /// Stops the session and removes the monitor.
    ///
    /// Takes `&self` so it works through an `Arc`, and is idempotent -- calling
    /// it twice, or after a drop-triggered teardown, is harmless.
    pub async fn close(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Stopping the remote-desktop session also stops the linked screen-cast
        // session; calling ScreenCast.Session.Stop directly is rejected.
        if let Err(e) = self.remote_desktop.stop().await {
            debug!(error = %e, "RemoteDesktop.Stop failed, trying ScreenCast.Stop");
            self.screen_cast.stop().await?;
        }
        info!("virtual monitor removed");
        Ok(())
    }

    // --- input -----------------------------------------------------------
    // Coordinates are in stream space (0..width, 0..height), so they map onto the
    // virtual monitor directly.

    pub async fn touch_down(&self, slot: u32, x: f64, y: f64) -> Result<()> {
        self.remote_desktop
            .notify_touch_down(&self.stream_path, slot, x, y)
            .await?;
        Ok(())
    }

    pub async fn touch_motion(&self, slot: u32, x: f64, y: f64) -> Result<()> {
        self.remote_desktop
            .notify_touch_motion(&self.stream_path, slot, x, y)
            .await?;
        Ok(())
    }

    pub async fn touch_up(&self, slot: u32) -> Result<()> {
        self.remote_desktop.notify_touch_up(slot).await?;
        Ok(())
    }

    pub async fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        self.remote_desktop
            .notify_pointer_motion_absolute(&self.stream_path, x, y)
            .await?;
        Ok(())
    }

    pub async fn pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        self.remote_desktop
            .notify_pointer_button(button, pressed)
            .await?;
        Ok(())
    }

    pub async fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        self.remote_desktop.notify_pointer_axis(dx, dy, 0).await?;
        Ok(())
    }

    pub async fn keyboard_keycode(&self, keycode: u32, pressed: bool) -> Result<()> {
        self.remote_desktop
            .notify_keyboard_keycode(keycode, pressed)
            .await?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Best effort: we cannot await here. Dropping the connection also makes
        // mutter reap the session, so the monitor does go away either way.
        debug!("Session dropped without close(); relying on peer disconnect");
    }
}

/// A remote-desktop session with no screen cast attached, for injecting input
/// into the session at large.
///
/// Keyboard and relative-pointer events are global rather than stream-relative,
/// so they need no virtual monitor. That makes this usable alongside a running
/// [`Session`] instead of competing with it for an output.
pub struct InputOnlySession {
    _conn: Connection,
    session: RemoteDesktopSessionProxy<'static>,
    stopped: std::sync::atomic::AtomicBool,
}

impl InputOnlySession {
    pub async fn open() -> Result<Self> {
        let conn = Connection::session().await?;
        let remote_desktop = RemoteDesktopProxy::new(&conn)
            .await
            .map_err(|_| Error::NoMutter)?;
        let path = remote_desktop.create_session().await?;
        let session = RemoteDesktopSessionProxy::new(&conn, path).await?;
        session.start().await?;
        Ok(Self {
            _conn: conn,
            session,
            stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Presses the modifiers, taps the key, then releases in reverse order.
    pub async fn send_chord(&self, chord: &Chord) -> Result<()> {
        for m in &chord.modifiers {
            self.session.notify_keyboard_keycode(*m, true).await?;
        }
        self.session
            .notify_keyboard_keycode(chord.key, true)
            .await?;
        self.session
            .notify_keyboard_keycode(chord.key, false)
            .await?;
        // Reverse order on release, mirroring how a human lets go of a chord.
        for m in chord.modifiers.iter().rev() {
            self.session.notify_keyboard_keycode(*m, false).await?;
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.session.stop().await?;
        Ok(())
    }
}

/// Lists connector names mutter currently knows about (e.g. `["DP-3", "HDMI-1"]`).
///
/// Used to populate the mirror-source picker.
pub async fn list_monitors() -> Result<Vec<String>> {
    use zvariant::OwnedValue;

    let conn = Connection::session().await?;
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await?;

    // GetCurrentState returns:
    //   u                            serial
    //   a((ssss)a(siiddada{sv})a{sv})  monitors
    //   a(iiduba(ssss)a{sv})           logical monitors
    //   a{sv}                          properties
    //
    // Each monitor's first field is (connector, vendor, product, serial); the
    // connector is all we need. The nested mode struct must still be spelled out
    // correctly or the whole deserialize fails.
    type MonitorSpec = (String, String, String, String);
    type Mode = (
        String,                      // mode id
        i32,                         // width
        i32,                         // height
        f64,                         // refresh rate
        f64,                         // preferred scale
        Vec<f64>,                    // supported scales
        HashMap<String, OwnedValue>, // properties
    );
    type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
    type LogicalMonitor = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<MonitorSpec>,
        HashMap<String, OwnedValue>,
    );

    let body = reply.body();
    let (_serial, monitors, _logical, _props): (
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        HashMap<String, OwnedValue>,
    ) = match body.deserialize() {
        Ok(v) => v,
        Err(e) => {
            // Not worth failing the whole app over: this only feeds the
            // mirror-source picker, and mutter's reply shape has changed before.
            warn!(error = %e, "could not parse DisplayConfig.GetCurrentState; mirror sources unavailable");
            return Ok(Vec::new());
        }
    };

    Ok(monitors
        .into_iter()
        .map(|((connector, ..), ..)| connector)
        .collect())
}
