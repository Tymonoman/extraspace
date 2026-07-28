//! Getting bytes between the host and the tablet over USB.
//!
//! Three `adb forward`s are set up rather than one. It costs almost nothing and
//! it removes head-of-line blocking: a touch event should never wait behind a
//! 30 KB video frame that is already half-written to the socket.
//!
//! Direction of travel is always the same -- the host *connects*, the tablet
//! *listens* on abstract unix sockets. That means the companion app can be
//! started first and simply wait, and it avoids needing `adb reverse`, which is
//! less reliable across reconnects.

use std::path::Path;
use std::time::Duration;

use tokio::net::TcpStream;
use tracing::{debug, info, warn};
use xs_proto::ports;

pub mod adb;
pub mod frame;

pub use adb::{Adb, Device, DeviceState};
pub use frame::{Frame, FrameReader, FrameWriter};

/// Android package of the companion app.
pub const PACKAGE: &str = "io.github.tymonoman.extraspace";
/// Activity that renders the mirrored display.
pub const ACTIVITY: &str = "io.github.tymonoman.extraspace/.MirrorActivity";

/// Abstract socket names the companion app listens on. Must match `Sockets.kt`.
pub mod sockets {
    pub const CONTROL: &str = "extraspace-control";
    pub const VIDEO: &str = "extraspace-video";
    pub const CAMERA: &str = "extraspace-camera";
}

/// How long to keep retrying the initial connect while the app starts up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Adb(#[from] adb::Error),

    #[error(transparent)]
    Frame(#[from] frame::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "the companion app did not accept a connection on port {port} within {}s. \
         It may have crashed on startup -- check `adb logcat -s extraspace`.",
        CONNECT_TIMEOUT.as_secs()
    )]
    ConnectTimeout { port: u16 },
}

pub type Result<T> = std::result::Result<T, Error>;

/// A connected tablet with all three channels established.
pub struct Transport {
    pub device: Device,
    pub control: TcpStream,
    pub video: TcpStream,
    pub camera: TcpStream,
    adb: Adb,
    simulated: bool,
}

/// The teardown half of a [`Transport`], kept after the sockets are handed out.
///
/// Splitting these apart lets the caller move each socket into its own task
/// while still holding something that can undo the adb forwards afterwards.
pub struct TransportHandle {
    pub device: Device,
    adb: Adb,
    /// True for the fake-tablet path, where there is nothing for adb to undo.
    simulated: bool,
}

impl TransportHandle {
    /// Removes the port forwards and stops the companion app.
    pub async fn disconnect(&self) {
        if self.simulated {
            debug!("simulated transport: nothing to tear down");
            return;
        }
        for port in [ports::CONTROL, ports::VIDEO, ports::CAMERA] {
            self.adb.remove_forward(&self.device.serial, port).await;
        }
        self.adb.force_stop(&self.device.serial, PACKAGE).await;
        debug!("transport torn down");
    }
}

/// Environment variable that swaps the real tablet for anything listening on the
/// three ports locally.
///
/// This exists so the host can be developed and tested without hardware --
/// see `cargo run -p xs-core --example fake_tablet`. It is checked at connect
/// time rather than behind a cargo feature so a release build can still use it.
pub const FAKE_TABLET_ENV: &str = "EXTRASPACE_FAKE_TABLET";

fn fake_tablet_requested() -> bool {
    std::env::var_os(FAKE_TABLET_ENV).is_some_and(|v| v != "0" && !v.is_empty())
}

impl Transport {
    /// Full connect sequence: find the device, make sure the companion app is
    /// installed and running, forward the ports, and connect all three channels.
    ///
    /// `apk` is optional -- when present and newer than what is installed, it is
    /// pushed automatically so the app and host can never drift out of sync.
    pub async fn connect(apk: Option<&Path>, apk_version: u32) -> Result<Self> {
        if fake_tablet_requested() {
            return Self::connect_fake().await;
        }
        let adb = Adb::find()?;
        let device = adb.require_device().await?;
        info!(device = %device.display_name(), serial = %device.serial, "tablet found");

        if let Some(apk) = apk {
            ensure_app_installed(&adb, &device.serial, apk, apk_version).await?;
        }

        // Forward first: the app needs somewhere to be reached even though it is
        // the one listening.
        adb.forward(&device.serial, ports::CONTROL, sockets::CONTROL).await?;
        adb.forward(&device.serial, ports::VIDEO, sockets::VIDEO).await?;
        adb.forward(&device.serial, ports::CAMERA, sockets::CAMERA).await?;

        // Restart the activity so we always talk to a fresh instance rather than
        // one left over from a previous run with stale sockets.
        adb.force_stop(&device.serial, PACKAGE).await;
        adb.start_activity(&device.serial, ACTIVITY).await?;

        // Control first, and only once it has actually spoken -- see
        // `connect_once_listening` for why connecting is not enough.
        let control = connect_once_listening(ports::CONTROL).await?;
        let video = connect_with_retry(ports::VIDEO).await?;
        let camera = connect_with_retry(ports::CAMERA).await?;
        info!("all three channels connected");

        Ok(Self {
            device,
            control,
            video,
            camera,
            adb,
            simulated: false,
        })
    }

    /// Connects to a stand-in tablet already listening on the three ports.
    ///
    /// No adb, no device, no APK -- just three TCP connections. Used by the
    /// `fake_tablet` example to exercise the whole host pipeline on one machine.
    async fn connect_fake() -> Result<Self> {
        info!("{FAKE_TABLET_ENV} is set: connecting to a local stand-in, not a real tablet");
        let control = connect_once_listening(ports::CONTROL).await?;
        let video = connect_with_retry(ports::VIDEO).await?;
        let camera = connect_with_retry(ports::CAMERA).await?;
        Ok(Self {
            device: Device {
                serial: "fake".into(),
                state: DeviceState::Ready,
                model: Some("Simulated_Tablet".into()),
            },
            control,
            video,
            camera,
            adb: Adb::find().unwrap_or_else(|_| Adb::none()),
            simulated: true,
        })
    }

    /// Splits into a teardown handle and the three sockets, so each can be moved
    /// into its own task.
    pub fn split(self) -> (TransportHandle, TcpStream, TcpStream, TcpStream) {
        (
            TransportHandle {
                device: self.device,
                adb: self.adb,
                simulated: self.simulated,
            },
            self.control,
            self.video,
            self.camera,
        )
    }

    /// Removes the port forwards. Called on teardown; failures are logged only.
    pub async fn disconnect(&self) {
        for port in [ports::CONTROL, ports::VIDEO, ports::CAMERA] {
            self.adb.remove_forward(&self.device.serial, port).await;
        }
        self.adb.force_stop(&self.device.serial, PACKAGE).await;
        debug!("transport torn down");
    }
}

/// Installs the companion app if missing or out of date.
async fn ensure_app_installed(
    adb: &Adb,
    serial: &str,
    apk: &Path,
    bundled_version: u32,
) -> Result<()> {
    match adb.package_version(serial, PACKAGE).await? {
        Some(installed) if installed >= bundled_version => {
            debug!(installed, "companion app is up to date");
        }
        Some(installed) => {
            info!(installed, bundled = bundled_version, "upgrading companion app");
            adb.install(serial, apk).await?;
        }
        None => {
            info!("companion app not installed, installing");
            adb.install(serial, apk).await?;
        }
    }
    Ok(())
}

/// Connects, and does not return until the peer has proven it is really there.
///
/// This exists because of a genuinely misleading `adb forward` behaviour: adb
/// accepts the *local* TCP connection whether or not anything is listening on the
/// device, and only then tries to open the remote socket. If that fails it simply
/// closes the connection. So `TcpStream::connect` succeeds on the very first try
/// even when the companion app has not finished starting, and the failure surfaces
/// milliseconds later as an unexplained EOF midway through the handshake.
///
/// Retrying on connection-refused therefore never fires. The only reliable signal
/// is bytes: the app writes `Hello` immediately on accept, so we peek for one byte
/// and treat silence or EOF as "not up yet".
async fn connect_once_listening(port: u16) -> Result<TcpStream> {
    /// How long to give the peer to say something before assuming it is a phantom.
    const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    let mut attempts = 0u32;
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            let _ = stream.set_nodelay(true);
            let mut probe = [0u8; 1];
            // peek leaves the byte in the socket buffer for the real reader.
            match tokio::time::timeout(PROBE_TIMEOUT, stream.peek(&mut probe)).await {
                Ok(Ok(n)) if n > 0 => {
                    debug!(port, attempts, "control channel connected and talking");
                    return Ok(stream);
                }
                // n == 0 is EOF: adb's phantom accept. Anything else means the app
                // is up but silent, which for the control channel also means not ready.
                _ => {}
            }
        }
        attempts += 1;
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::ConnectTimeout { port });
        }
        tokio::time::sleep(CONNECT_RETRY_DELAY).await;
    }
}

/// Connects to a forwarded port for a channel the peer does not speak on first.
///
/// Only safe to use *after* [`connect_once_listening`] has confirmed the app is
/// running, since it cannot distinguish a real connection from adb's phantom accept.
async fn connect_with_retry(port: u16) -> Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    let mut attempts = 0u32;
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => {
                // Disable Nagle: we already batch a frame into one write, and
                // waiting to coalesce would add latency to every touch event.
                if let Err(e) = stream.set_nodelay(true) {
                    warn!(error = %e, "could not disable Nagle on port {port}");
                }
                debug!(port, attempts, "channel connected");
                return Ok(stream);
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                attempts += 1;
                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(_) => return Err(Error::ConnectTimeout { port }),
        }
    }
}
