//! The engine: everything that happens between "user flicks the switch" and
//! "desktop appears on the tablet".
//!
//! The GTK front end never touches a socket, a pipeline or a D-Bus session. It
//! sends [`Command`]s and renders [`Event`]s. That keeps all the blocking and
//! async work off the main loop, and means the whole engine can be driven from a
//! test or a future CLI without a window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, warn};

pub mod adaptive;
mod session;

pub use adaptive::{AdaptiveController, BitrateBounds, HealthSample};
pub use session::{virtual_size_for, DisplayMode, SessionConfig};

/// Events per second the tablet reports and the controller consumes.
pub const STATS_HZ: u64 = 2;

/// What the UI asks the engine to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// Look for a tablet and start streaming.
    Connect,
    /// Tear everything down.
    Disconnect,
    /// Change the logical scale, e.g. 1.5. Recreates the virtual monitor.
    SetScale(f64),
    /// Switch between extending and mirroring.
    SetMode(DisplayMode),
    /// Change the quality envelope the adaptive controller works within.
    SetBitrateBounds(BitrateBounds),
    /// Start or stop camera passthrough.
    SetCamera { enabled: bool, camera_id: String },
    /// Shut the engine thread down.
    Shutdown,
}

/// Where the engine currently is. Drives what the UI shows.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Nothing running.
    Idle,
    /// No tablet plugged in, or adb cannot see one.
    NoTablet,
    /// Tablet present but the user has not accepted the USB-debugging prompt.
    /// Worth its own state because it is the single most common first-run failure
    /// and the fix is entirely in the user's hands.
    Unauthorized { device: String },
    /// Installing the companion app, forwarding ports, negotiating.
    Connecting { step: String },
    /// Running.
    Streaming {
        device: String,
        width: u32,
        height: u32,
        encoder: String,
    },
    /// Stopped because of an error. Carries something the user can act on.
    Failed { message: String },
}

/// Live numbers for the statistics panel.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub bitrate_kbps: u32,
    pub fps: f64,
    pub latency_ms: f64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub decode_queue_depth: u32,
}

#[derive(Debug, Clone)]
pub enum Event {
    State(State),
    Stats(Stats),
    /// Non-fatal; shown as a toast rather than replacing the whole view.
    Warning(String),
}

/// Handle the UI holds. Cloneable and cheap.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<Event>,
}

impl EngineHandle {
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            warn!("engine is gone; command dropped");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

/// Starts the engine on its own runtime in a background thread.
///
/// A dedicated thread rather than sharing the GTK main loop: GStreamer and zbus
/// both want to block, and a stalled encoder must never freeze the window.
pub fn spawn(config: SessionConfig) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, _) = broadcast::channel(64);
    let handle = EngineHandle {
        commands: cmd_tx,
        events: event_tx.clone(),
    };

    std::thread::Builder::new()
        .name("xs-engine".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(error = %e, "could not start engine runtime");
                    let _ = event_tx.send(Event::State(State::Failed {
                        message: format!("could not start engine: {e}"),
                    }));
                    return;
                }
            };
            runtime.block_on(session::run(config, cmd_rx, event_tx));
            debug!("engine thread finished");
        })
        .expect("spawning the engine thread should not fail");

    handle
}

/// Shared helper so both the engine and the UI format rates the same way.
pub fn format_bitrate(kbps: u32) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbps", kbps as f64 / 1000.0)
    } else {
        format!("{kbps} kbps")
    }
}

/// Tracks frames-per-second over a short sliding window.
pub(crate) struct FpsCounter {
    window: Duration,
    ticks: Vec<Instant>,
}

impl FpsCounter {
    pub fn new() -> Self {
        Self {
            window: Duration::from_secs(2),
            ticks: Vec::with_capacity(256),
        }
    }

    pub fn tick(&mut self, now: Instant) {
        self.ticks.push(now);
        let cutoff = now - self.window;
        self.ticks.retain(|t| *t >= cutoff);
    }

    pub fn fps(&self) -> f64 {
        if self.ticks.len() < 2 {
            return 0.0;
        }
        let span = self
            .ticks
            .last()
            .unwrap()
            .duration_since(*self.ticks.first().unwrap());
        if span.is_zero() {
            return 0.0;
        }
        (self.ticks.len() - 1) as f64 / span.as_secs_f64()
    }
}

/// Shared counters the engine updates and the UI reads.
#[derive(Debug, Default)]
pub struct SharedStats {
    pub inner: std::sync::Mutex<Stats>,
}

impl SharedStats {
    pub fn get(&self) -> Stats {
        self.inner.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

pub type SharedStatsRef = Arc<SharedStats>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bitrate_readably() {
        assert_eq!(format_bitrate(15_000), "15.0 Mbps");
        assert_eq!(format_bitrate(6_500), "6.5 Mbps");
        assert_eq!(format_bitrate(800), "800 kbps");
    }

    #[test]
    fn fps_counter_measures_a_steady_stream() {
        let mut c = FpsCounter::new();
        let t0 = Instant::now();
        // 60 ticks at 16.67ms apart.
        for i in 0..60 {
            c.tick(t0 + Duration::from_micros(16_667 * i));
        }
        assert!((c.fps() - 60.0).abs() < 1.0, "got {}", c.fps());
    }

    #[test]
    fn fps_counter_is_zero_before_it_has_evidence() {
        let mut c = FpsCounter::new();
        assert_eq!(c.fps(), 0.0);
        c.tick(Instant::now());
        assert_eq!(c.fps(), 0.0);
    }

    #[test]
    fn fps_counter_forgets_old_ticks() {
        let mut c = FpsCounter::new();
        let t0 = Instant::now();
        for i in 0..60 {
            c.tick(t0 + Duration::from_micros(16_667 * i));
        }
        // Ten seconds later, a single tick: the old window must have aged out.
        c.tick(t0 + Duration::from_secs(10));
        assert_eq!(c.fps(), 0.0);
    }
}
