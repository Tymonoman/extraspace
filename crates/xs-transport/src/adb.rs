//! Driving the `adb` command-line tool.
//!
//! We shell out rather than speak the adb server protocol directly. The protocol
//! is not hard, but `adb` is already a required dependency for USB debugging to
//! work at all, and reimplementing it would buy nothing.
//!
//! The one state worth handling carefully is [`DeviceState::Unauthorized`]: it is
//! by far the most common reason a first run fails, and it is entirely fixable by
//! the user, so it gets its own variant and its own error message rather than
//! being lumped in with "no device".

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("adb not found. Install it with: sudo dnf install android-tools")]
    AdbNotFound,

    #[error("adb {command} failed (exit {code}): {stderr}")]
    CommandFailed {
        command: String,
        code: i32,
        stderr: String,
    },

    #[error("io error running adb: {0}")]
    Io(#[from] std::io::Error),

    #[error("no Android device connected over USB")]
    NoDevice,

    #[error(
        "tablet '{0}' is connected but not authorised. Unlock it and accept the \
         'Allow USB debugging?' prompt, ticking 'Always allow from this computer'."
    )]
    Unauthorized(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Ready to use.
    Ready,
    /// Connected, but the user has not accepted the USB-debugging prompt.
    Unauthorized,
    /// Seen but not usable (booting, or a stale entry).
    Offline,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub serial: String,
    pub state: DeviceState,
    /// From `adb devices -l`, e.g. `T_Tablet`. Shown in the UI.
    pub model: Option<String>,
}

impl Device {
    pub fn display_name(&self) -> String {
        self.model
            .as_ref()
            .map(|m| m.replace('_', " "))
            .unwrap_or_else(|| self.serial.clone())
    }
}

#[derive(Debug, Clone)]
pub struct Adb {
    binary: PathBuf,
}

impl Adb {
    /// Locates `adb` on `PATH`.
    pub fn find() -> Result<Self> {
        which("adb").map(|binary| Self { binary }).ok_or(Error::AdbNotFound)
    }

    /// A handle that points at nothing, for the fake-tablet path where teardown
    /// has no forwards to remove. Every command against it simply fails, which
    /// the teardown path already treats as non-fatal.
    pub fn none() -> Self {
        Self {
            binary: PathBuf::from("/nonexistent/adb"),
        }
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        debug!(args = ?args, "adb");
        let output = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await?;

        if !output.status.success() {
            return Err(Error::CommandFailed {
                command: args.join(" "),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Runs a command against a specific device.
    async fn run_on(&self, serial: &str, args: &[&str]) -> Result<String> {
        let mut full = vec!["-s", serial];
        full.extend_from_slice(args);
        self.run(&full).await
    }

    /// All devices adb currently knows about, in any state.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let out = self.run(&["devices", "-l"]).await?;
        Ok(out
            .lines()
            .skip(1) // "List of devices attached"
            .filter(|l| !l.trim().is_empty())
            .filter_map(parse_device_line)
            .collect())
    }

    /// The single usable device, with actionable errors for the common failures.
    pub async fn require_device(&self) -> Result<Device> {
        let devices = self.devices().await?;
        if let Some(d) = devices.iter().find(|d| d.state == DeviceState::Ready) {
            return Ok(d.clone());
        }
        if let Some(d) = devices.iter().find(|d| d.state == DeviceState::Unauthorized) {
            return Err(Error::Unauthorized(d.display_name()));
        }
        Err(Error::NoDevice)
    }

    /// Forwards a host TCP port to an abstract unix socket on the device.
    ///
    /// This is the direction scrcpy uses and the reason throughput is good: the
    /// bytes travel over the USB bulk endpoints adb already owns.
    pub async fn forward(&self, serial: &str, local_port: u16, remote_socket: &str) -> Result<()> {
        let local = format!("tcp:{local_port}");
        let remote = format!("localabstract:{remote_socket}");
        self.run_on(serial, &["forward", &local, &remote]).await?;
        debug!(local_port, remote_socket, "adb forward established");
        Ok(())
    }

    /// Removes forwards we created. Best-effort: failures here are not worth
    /// failing a teardown over.
    pub async fn remove_forward(&self, serial: &str, local_port: u16) {
        let local = format!("tcp:{local_port}");
        if let Err(e) = self.run_on(serial, &["forward", "--remove", &local]).await {
            debug!(error = %e, local_port, "could not remove forward");
        }
    }

    /// `versionCode` of an installed package, or `None` if it is not installed.
    pub async fn package_version(&self, serial: &str, package: &str) -> Result<Option<u32>> {
        let out = match self
            .run_on(serial, &["shell", "dumpsys", "package", package])
            .await
        {
            Ok(o) => o,
            // dumpsys exits non-zero for unknown packages on some builds.
            Err(_) => return Ok(None),
        };
        if !out.contains(package) {
            return Ok(None);
        }
        Ok(out
            .lines()
            .find_map(|l| l.trim().strip_prefix("versionCode="))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok()))
    }

    /// Installs or upgrades the companion app.
    pub async fn install(&self, serial: &str, apk: &Path) -> Result<()> {
        let path = apk.to_string_lossy();
        info!(apk = %path, "installing companion app");
        // -r replace, -g grant runtime permissions (camera), -d allow downgrade
        // so a dev build can replace a newer store build.
        self.run_on(serial, &["install", "-r", "-g", "-d", &path]).await?;
        info!("companion app installed");
        Ok(())
    }

    /// Launches an activity, e.g. `io.github.tymonoman.extraspace/.MirrorActivity`.
    pub async fn start_activity(&self, serial: &str, component: &str) -> Result<()> {
        self.run_on(serial, &["shell", "am", "start", "-n", component]).await?;
        debug!(component, "activity started");
        Ok(())
    }

    /// Force-stops the companion app.
    pub async fn force_stop(&self, serial: &str, package: &str) {
        if let Err(e) = self.run_on(serial, &["shell", "am", "force-stop", package]).await {
            warn!(error = %e, "could not force-stop companion app");
        }
    }

    /// Device screen size in pixels, from `wm size`, as a fallback if the
    /// companion app has not reported it yet.
    pub async fn screen_size(&self, serial: &str) -> Result<Option<(u32, u32)>> {
        let out = self.run_on(serial, &["shell", "wm", "size"]).await?;
        // "Physical size: 1200x2000" (portrait-native on most tablets)
        Ok(out.lines().find_map(|l| {
            let (w, h) = l.split(':').nth(1)?.trim().split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        }))
    }
}

fn parse_device_line(line: &str) -> Option<Device> {
    let mut parts = line.split_whitespace();
    let serial = parts.next()?.to_string();
    let state = match parts.next()? {
        "device" => DeviceState::Ready,
        "unauthorized" => DeviceState::Unauthorized,
        _ => DeviceState::Offline,
    };
    // Remaining fields are `key:value`, e.g. `model:T_Tablet`.
    let model = line
        .split_whitespace()
        .find_map(|f| f.strip_prefix("model:"))
        .map(str::to_string);
    Some(Device { serial, state, model })
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_ready_device() {
        let d = parse_device_line(
            "R3CT90XMPLZ       device usb:1-6.1 product:T_Tablet model:T_Tablet",
        )
        .unwrap();
        assert_eq!(d.serial, "R3CT90XMPLZ");
        assert_eq!(d.state, DeviceState::Ready);
        assert_eq!(d.display_name(), "T Tablet");
    }

    #[test]
    fn parses_the_unauthorized_state_we_actually_hit() {
        // Shape of a real unauthorized line; the serial is anonymised.
        let d = parse_device_line("R3CT90XMPLZ    unauthorized usb:1-6.1 transport_id:1")
            .unwrap();
        assert_eq!(d.state, DeviceState::Unauthorized);
        // No model is reported until authorised, so the serial has to do.
        assert_eq!(d.display_name(), "R3CT90XMPLZ");
    }

    #[test]
    fn treats_unknown_states_as_offline() {
        let d = parse_device_line("emulator-5554   bootloader").unwrap();
        assert_eq!(d.state, DeviceState::Offline);
    }

    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_device_line("").is_none());
        assert!(parse_device_line("onlyserial").is_none());
    }
}
