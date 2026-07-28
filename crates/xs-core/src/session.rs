//! The orchestration loop.
//!
//! One `run` task owns the whole lifecycle; everything else is a child task it
//! can abort. Teardown is therefore simple: cancel the children, close the mutter
//! session, undo the adb forwards.
//!
//! Shared state is kept to three things, each behind an `Arc` because more than
//! one task genuinely needs them for its whole life:
//!
//! * the mutter session -- the touch task injects into it, teardown closes it;
//! * the video pipeline -- the bitrate task retunes it, teardown stops it;
//! * the adaptive controller -- the stats task feeds it, the UI retunes its bounds.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use xs_mutter::{CaptureSource, CursorMode, DisplayConfig};
use xs_proto::{flags, Channel, ControlKind, Hello, TouchAction, TouchEvent, VideoConfig};
use xs_transport::{FrameReader, FrameWriter, Transport, TransportHandle};
use xs_video::{VideoConfig as PipelineConfig, VideoPipeline};

use crate::adaptive::{AdaptiveController, BitrateBounds, HealthSample};
use crate::{Command, Event, FpsCounter, State, Stats};

/// How often we probe round-trip latency.
const PING_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// A new monitor: the desktop gets bigger.
    Extend,
    /// A copy of an existing monitor.
    Mirror,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub mode: DisplayMode,
    /// Logical scale. See [`virtual_size_for`] for what this actually does.
    pub scale: f64,
    pub framerate: u32,
    pub bounds: BitrateBounds,
    /// Connector to mirror when [`DisplayMode::Mirror`], e.g. `DP-3`.
    pub mirror_source: Option<String>,
    /// Companion APK to push, if bundled with the build.
    pub apk_path: Option<PathBuf>,
    pub apk_version: u32,
    pub camera_enabled: bool,
    pub camera_id: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Extend,
            scale: 1.5,
            framerate: 60,
            bounds: BitrateBounds::default(),
            mirror_source: None,
            apk_path: None,
            apk_version: 1,
            camera_enabled: false,
            camera_id: "0".into(),
        }
    }
}

/// Turns a panel size and a scale into the resolution to actually stream.
///
/// Rather than asking mutter to apply a scale factor -- which `RecordVirtual` has
/// no way to express -- the virtual monitor is created *smaller* and the tablet
/// upscales it to fill the panel. A 2000x1200 panel at 1.5x becomes a 1332x800
/// monitor, so everything on it is drawn 1.5x larger. Text softens slightly from
/// the upscale, which is a fair trade for readable UI on a 10.4" screen, and it
/// lowers the bitrate needed at the same time.
///
/// Dimensions are forced even because H.264 4:2:0 chroma cannot represent an odd
/// width or height; encoders either reject it or silently pad.
pub fn virtual_size_for(panel_width: u32, panel_height: u32, scale: f64) -> (u32, u32) {
    let scale = scale.clamp(1.0, 3.0);
    let w = ((panel_width as f64 / scale).round() as u32).max(640);
    let h = ((panel_height as f64 / scale).round() as u32).max(480);
    (w & !1, h & !1)
}

type SharedWriter = Arc<Mutex<FrameWriter<OwnedWriteHalf>>>;

/// Everything belonging to one live connection.
struct Active {
    transport: TransportHandle,
    /// Held open while the camera is off, so the channel survives until the user
    /// enables it. Closing it would take the whole adb forward down with it.
    _idle_camera: Option<tokio::net::TcpStream>,
    mutter: Arc<xs_mutter::Session>,
    pipeline: Arc<VideoPipeline>,
    controller: Arc<Mutex<AdaptiveController>>,
    control_writer: SharedWriter,
    tasks: Vec<JoinHandle<()>>,
    device_name: String,
    encoder_name: String,
    width: u32,
    height: u32,
}

impl Active {
    fn streaming_state(&self) -> State {
        State::Streaming {
            device: self.device_name.clone(),
            width: self.width,
            height: self.height,
            encoder: self.encoder_name.clone(),
        }
    }

    async fn shutdown(mut self) {
        // Stop the tasks first, so nothing is still pushing frames or injecting
        // input while the session underneath it disappears.
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.pipeline.stop();
        self.transport.disconnect().await;
        if let Err(e) = self.mutter.close().await {
            debug!(error = %e, "mutter session close");
        }
        info!("session stopped");
    }
}

pub async fn run(
    mut config: SessionConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<Event>,
) {
    let mut active: Option<Active> = None;
    let emit = |state: State| {
        let _ = events.send(Event::State(state));
    };
    emit(State::Idle);

    while let Some(command) = commands.recv().await {
        match command {
            Command::Connect => {
                if active.is_some() {
                    continue;
                }
                active = try_connect(&config, &events).await;
            }

            Command::Disconnect => {
                if let Some(session) = active.take() {
                    session.shutdown().await;
                }
                emit(State::Idle);
            }

            // Scale and mode both change the monitor's geometry, so the session
            // must be rebuilt. It takes well under a second, so the UI can present
            // it as a live change rather than a restart.
            Command::SetScale(scale) => {
                config.scale = scale;
                if let Some(session) = active.take() {
                    session.shutdown().await;
                    active = try_connect(&config, &events).await;
                }
            }

            Command::SetMode(mode) => {
                config.mode = mode;
                if let Some(session) = active.take() {
                    session.shutdown().await;
                    active = try_connect(&config, &events).await;
                }
            }

            Command::SetBitrateBounds(bounds) => {
                config.bounds = bounds;
                if let Some(session) = active.as_ref() {
                    let mut controller = session.controller.lock().await;
                    controller.set_bounds(bounds);
                    session.pipeline.set_bitrate(controller.current_kbps());
                }
            }

            Command::SetCamera { enabled, camera_id } => {
                config.camera_enabled = enabled;
                config.camera_id = camera_id.clone();
                if let Some(session) = active.as_ref() {
                    if let Err(e) =
                        send_camera_control(&session.control_writer, enabled, &camera_id).await
                    {
                        warn!(error = %e, "camera control failed");
                        let _ = events.send(Event::Warning(format!("Camera: {e}")));
                    }
                }
            }

            Command::Shutdown => {
                if let Some(session) = active.take() {
                    session.shutdown().await;
                }
                break;
            }
        }
    }
    debug!("engine loop exited");
}

/// Connects and reports the outcome, returning the session on success.
async fn try_connect(config: &SessionConfig, events: &broadcast::Sender<Event>) -> Option<Active> {
    match connect(config, events).await {
        Ok(session) => {
            let _ = events.send(Event::State(session.streaming_state()));
            Some(session)
        }
        Err(e) => {
            error!(error = %e, "connect failed");
            let _ = events.send(Event::State(state_for_error(&e)));
            None
        }
    }
}

fn state_for_error(e: &anyhow::Error) -> State {
    // Surface the two failures the user can actually fix as their own states,
    // rather than burying them in a generic error string.
    match e.downcast_ref::<xs_transport::adb::Error>() {
        Some(xs_transport::adb::Error::Unauthorized(device)) => State::Unauthorized {
            device: device.clone(),
        },
        Some(xs_transport::adb::Error::NoDevice) => State::NoTablet,
        _ => State::Failed {
            message: format!("{e:#}"),
        },
    }
}

async fn connect(
    config: &SessionConfig,
    events: &broadcast::Sender<Event>,
) -> anyhow::Result<Active> {
    let step = |s: &str| {
        let _ = events.send(Event::State(State::Connecting {
            step: s.to_string(),
        }));
    };

    step("Looking for your tablet…");
    let transport = Transport::connect(config.apk_path.as_deref(), config.apk_version).await?;
    let device_name = transport.device.display_name();
    let (handle, control, video, camera) = transport.split();

    // Only the control channel is bidirectional, so only it is split. This is not
    // tidiness: dropping tokio's `OwnedWriteHalf` calls shutdown(Write), and adb's
    // forwarder tears down the entire unix socket to the device when either
    // direction closes. Splitting the camera stream and dropping its write half
    // therefore killed the channel, and the tablet's very next frame hit EPIPE.
    let (control_rx, control_tx) = control.into_split();

    let mut control_reader = FrameReader::new(control_rx);
    let control_writer: SharedWriter = Arc::new(Mutex::new(FrameWriter::new(control_tx)));

    step("Waiting for the tablet to introduce itself…");
    let hello = read_hello(&mut control_reader).await?;
    info!(
        device = %hello.device_name,
        panel = format!("{}x{}", hello.width, hello.height),
        android = %hello.android_release,
        "tablet said hello"
    );

    let (width, height) = virtual_size_for(hello.width, hello.height, config.scale);

    step("Creating the display…");
    let source = match (config.mode, &config.mirror_source) {
        (DisplayMode::Mirror, Some(connector)) => CaptureSource::Monitor(connector.clone()),
        (DisplayMode::Mirror, None) => {
            // Asked to mirror with nothing chosen: use the first monitor rather
            // than failing outright.
            match xs_mutter::list_monitors().await.unwrap_or_default().first() {
                Some(c) => CaptureSource::Monitor(c.clone()),
                None => CaptureSource::Virtual,
            }
        }
        (DisplayMode::Extend, _) => CaptureSource::Virtual,
    };

    let mutter = Arc::new(
        xs_mutter::Session::open(DisplayConfig {
            width,
            height,
            refresh_rate: config.framerate as f64,
            cursor_mode: CursorMode::Embedded,
            source,
        })
        .await?,
    );

    step("Starting the video pipeline…");
    let start_kbps = starting_bitrate(width, height, config.framerate, config.bounds);
    let (pipeline, mut frames) = VideoPipeline::new(
        mutter.node_id(),
        PipelineConfig {
            width,
            height,
            framerate: config.framerate,
            bitrate_kbps: start_kbps,
        },
    )?;
    let encoder_name = pipeline.encoder().human_name().to_string();
    pipeline.start()?;
    let pipeline = Arc::new(pipeline);

    // Tell the tablet what is coming before the first frame arrives.
    let video_config = VideoConfig {
        width,
        height,
        framerate: config.framerate,
        bitrate_kbps: start_kbps,
        codec: "h264".into(),
    };
    control_writer
        .lock()
        .await
        .write_frame(
            Channel::Control,
            ControlKind::VideoConfig as u8,
            0,
            0,
            serde_json::to_string(&video_config)?.as_bytes(),
        )
        .await?;

    // The tablet cannot decode anything until it sees a keyframe, and on an idle
    // desktop the next scheduled one can be seconds away. Ask for one now so the
    // first image appears immediately rather than whenever the screen next changes.
    pipeline.request_keyframe();

    if config.camera_enabled {
        send_camera_control(&control_writer, true, &config.camera_id).await?;
    }

    let controller = Arc::new(Mutex::new(AdaptiveController::new(
        config.bounds,
        start_kbps,
    )));
    let (bitrate_tx, mut bitrate_rx) = mpsc::unbounded_channel::<u32>();
    // Round-trip latency in microseconds, written by the pong handler and read by
    // the stats handler. Atomic rather than a channel: only the freshest value
    // matters and a stale one is never worth waiting for.
    let rtt_us = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();

    // --- video out -------------------------------------------------------
    let mut video_writer = FrameWriter::new(video);
    tasks.push(tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            let flag = if frame.keyframe { flags::KEYFRAME } else { 0 };
            if let Err(e) = video_writer
                .write_frame(Channel::VideoDown, 0, flag, frame.pts_us, &frame.data)
                .await
            {
                debug!(error = %e, "video channel closed");
                break;
            }
        }
    }));

    // --- ping: measure real round-trip latency ---------------------------
    {
        let writer = Arc::clone(&control_writer);
        tasks.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PING_INTERVAL);
            loop {
                ticker.tick().await;
                let sent = monotonic_us();
                let result = writer
                    .lock()
                    .await
                    .write_frame(Channel::Control, ControlKind::Ping as u8, 0, sent, &[])
                    .await;
                if result.is_err() {
                    break;
                }
            }
        }));
    }

    // --- control in: touch, stats and pongs ------------------------------
    {
        let events = events.clone();
        let mutter = Arc::clone(&mutter);
        let stats_source = Arc::clone(pipeline.stats());
        let controller = Arc::clone(&controller);
        let rtt_us = Arc::clone(&rtt_us);
        let bitrate_tx = bitrate_tx.clone();

        tasks.push(tokio::spawn(async move {
            let mut fps = FpsCounter::new();
            let mut last_dropped = 0u64;
            let mut last_bytes = 0u64;
            let mut last_sample_at = Instant::now();

            loop {
                let frame = match control_reader.read_frame().await {
                    Ok(f) => f,
                    Err(e) => {
                        debug!(error = %e, "control channel closed");
                        break;
                    }
                };

                match frame.channel() {
                    Channel::Touch => {
                        let Some(touch) = TouchEvent::decode(&frame.payload) else {
                            warn!("malformed touch event");
                            continue;
                        };
                        // Log the ends of a gesture but not the motion between:
                        // motion arrives at up to 120 Hz per finger and would
                        // drown out everything else.
                        if !matches!(touch.action, TouchAction::Motion) {
                            debug!(
                                action = ?touch.action,
                                slot = touch.slot,
                                x = touch.x,
                                y = touch.y,
                                "touch"
                            );
                        }
                        let result = match touch.action {
                            TouchAction::Down => {
                                mutter.touch_down(touch.slot, touch.x, touch.y).await
                            }
                            TouchAction::Motion => {
                                mutter.touch_motion(touch.slot, touch.x, touch.y).await
                            }
                            TouchAction::Up => mutter.touch_up(touch.slot).await,
                        };
                        if let Err(e) = result {
                            warn!(error = %e, "touch injection failed");
                        }
                    }

                    Channel::Control if frame.header.kind == ControlKind::Pong as u8 => {
                        // The tablet echoes our timestamp verbatim, so this is a
                        // true round trip and needs no clock synchronisation.
                        let elapsed = monotonic_us().saturating_sub(frame.header.pts_us);
                        rtt_us.store(elapsed, Ordering::Relaxed);
                    }

                    Channel::Control if frame.header.kind == ControlKind::Stats as u8 => {
                        let Ok(device) = serde_json::from_slice::<xs_proto::Stats>(&frame.payload)
                        else {
                            continue;
                        };

                        let now = Instant::now();
                        let (encoded, dropped, bytes) = stats_source.snapshot();
                        fps.tick(now);

                        let elapsed = now.duration_since(last_sample_at).as_secs_f64().max(0.001);
                        last_sample_at = now;

                        let total_dropped = dropped + device.frames_dropped;
                        let rtt = Duration::from_micros(rtt_us.load(Ordering::Relaxed));
                        let sample = HealthSample {
                            decode_queue_depth: device.decode_queue_depth,
                            dropped_delta: total_dropped.saturating_sub(last_dropped),
                            rtt,
                        };
                        last_dropped = total_dropped;

                        // Log the inputs, not just the decision: when the bitrate
                        // walks somewhere surprising, the only useful question is
                        // which of the three signals drove it.
                        debug!(
                            queue = sample.decode_queue_depth,
                            drops = sample.dropped_delta,
                            rtt_ms = sample.rtt.as_millis() as u64,
                            host_dropped = dropped,
                            device_dropped = device.frames_dropped,
                            encoded,
                            "health sample"
                        );

                        if let Some(new_kbps) = controller.lock().await.observe(sample, now) {
                            info!(new_kbps, "adapting bitrate");
                            let _ = bitrate_tx.send(new_kbps);
                        }

                        // Bitrate measured from bytes actually produced, not the
                        // number we asked the encoder for.
                        let bitrate_kbps = ((bytes.saturating_sub(last_bytes)) as f64 * 8.0
                            / 1000.0
                            / elapsed) as u32;
                        last_bytes = bytes;

                        let _ = events.send(Event::Stats(Stats {
                            bitrate_kbps,
                            fps: fps.fps(),
                            latency_ms: rtt.as_secs_f64() * 1000.0,
                            frames_encoded: encoded,
                            frames_dropped: total_dropped,
                            decode_queue_depth: device.decode_queue_depth,
                        }));
                    }

                    _ => debug!(kind = frame.header.kind, "unhandled control frame"),
                }
            }
        }));
    }

    // --- apply adaptive bitrate ------------------------------------------
    {
        let pipeline = Arc::clone(&pipeline);
        tasks.push(tokio::spawn(async move {
            while let Some(kbps) = bitrate_rx.recv().await {
                pipeline.set_bitrate(kbps);
            }
        }));
    }

    // --- camera in -------------------------------------------------------
    // The socket is held either way. Letting it drop when the camera is off would
    // close the channel, and the tablet would then fail the moment the user turns
    // the camera on mid-session.
    let mut idle_camera = None;
    if config.camera_enabled {
        let events = events.clone();
        let mut camera_reader = FrameReader::new(camera);
        tasks.push(tokio::spawn(async move {
            let mut writer = match xs_camera::V4l2Writer::open_default() {
                Ok(w) => w,
                Err(e) => {
                    warn!(error = %e, "virtual camera unavailable");
                    let _ = events.send(Event::Warning(format!("Camera: {e}")));
                    return;
                }
            };
            loop {
                match camera_reader.read_frame().await {
                    Ok(frame) => {
                        let is_config = frame.header.flags & flags::CODEC_CONFIG != 0;
                        if let Err(e) = writer.push(&frame.payload, frame.header.pts_us, is_config)
                        {
                            warn!(error = %e, "writing to the virtual camera failed");
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "camera channel closed");
                        break;
                    }
                }
            }
        }));
    } else {
        idle_camera = Some(camera);
    }

    Ok(Active {
        transport: handle,
        _idle_camera: idle_camera,
        mutter,
        pipeline,
        controller,
        control_writer,
        tasks,
        device_name,
        encoder_name,
        width,
        height,
    })
}

/// Reads frames until the tablet's `Hello` turns up.
async fn read_hello<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
) -> anyhow::Result<Hello> {
    // A few frames of slack in case anything is queued ahead of it.
    for _ in 0..8 {
        let frame = reader.read_frame().await?;
        if frame.header.channel == Channel::Control && frame.header.kind == ControlKind::Hello as u8
        {
            let hello: Hello = serde_json::from_slice(&frame.payload)?;
            anyhow::ensure!(
                hello.protocol_version == xs_proto::PROTOCOL_VERSION,
                "the tablet app speaks protocol v{} but this build speaks v{}. \
                 Reinstall the companion app.",
                hello.protocol_version,
                xs_proto::PROTOCOL_VERSION
            );
            return Ok(hello);
        }
    }
    anyhow::bail!("tablet never sent a Hello message")
}

async fn send_camera_control(
    writer: &SharedWriter,
    enabled: bool,
    camera_id: &str,
) -> anyhow::Result<()> {
    let control = xs_proto::CameraControl {
        enabled,
        camera_id: camera_id.to_string(),
        width: 1920,
        height: 1080,
        framerate: 30,
        bitrate_kbps: 8_000,
    };
    writer
        .lock()
        .await
        .write_frame(
            Channel::Control,
            ControlKind::CameraControl as u8,
            0,
            0,
            serde_json::to_string(&control)?.as_bytes(),
        )
        .await?;
    Ok(())
}

/// Microseconds since an arbitrary fixed point. Only differences are meaningful,
/// which is all ping/pong needs -- and it sidesteps clock skew entirely.
fn monotonic_us() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// A sensible opening bitrate, so the first second does not look terrible while
/// the controller finds its level.
///
/// 0.2 bits per pixel per frame. The obvious 0.1 was measured starting a
/// 1332x800@60 session at 6393 kbit/s -- barely above the 6000 floor, so the
/// controller had nowhere to go but down and sat there. Screen content is mostly
/// static and compresses well, but text and sharp edges are exactly what suffers
/// when you starve it, and USB 2.0 has roughly 15x the headroom needed anyway.
const BITS_PER_PIXEL_PER_FRAME: f64 = 0.2;

fn starting_bitrate(width: u32, height: u32, framerate: u32, bounds: BitrateBounds) -> u32 {
    let pixels_per_second = width as f64 * height as f64 * framerate as f64;
    let kbps = (pixels_per_second * BITS_PER_PIXEL_PER_FRAME / 1000.0) as u32;
    kbps.clamp(bounds.min_kbps, bounds.max_kbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_scale_streams_the_full_panel() {
        assert_eq!(virtual_size_for(2000, 1200, 1.0), (2000, 1200));
    }

    #[test]
    fn scaling_shrinks_the_monitor_so_ui_looks_bigger() {
        // The T Tablet's panel at the default 1.5x.
        assert_eq!(virtual_size_for(2000, 1200, 1.5), (1332, 800));
        assert_eq!(virtual_size_for(2000, 1200, 2.0), (1000, 600));
    }

    #[test]
    fn dimensions_are_always_even_for_h264() {
        // 4:2:0 chroma cannot represent odd dimensions; encoders fail or pad.
        for scale in [1.0, 1.1, 1.25, 1.3, 1.5, 1.75, 2.0, 2.3, 3.0] {
            let (w, h) = virtual_size_for(2000, 1200, scale);
            assert_eq!(w % 2, 0, "width {w} odd at scale {scale}");
            assert_eq!(h % 2, 0, "height {h} odd at scale {scale}");
        }
    }

    #[test]
    fn absurd_scales_are_clamped_rather_than_producing_a_useless_monitor() {
        let (w, h) = virtual_size_for(2000, 1200, 99.0);
        assert!(w >= 640 && h >= 480, "got {w}x{h}");
        // Below 1.0 would mean streaming more pixels than the panel has.
        assert_eq!(virtual_size_for(2000, 1200, 0.1), (2000, 1200));
    }

    #[test]
    fn starting_bitrate_is_reasonable_and_bounded() {
        let bounds = BitrateBounds::default();
        let kbps = starting_bitrate(1332, 800, 60, bounds);
        assert!(
            (bounds.min_kbps..=bounds.max_kbps).contains(&kbps),
            "got {kbps}"
        );
        // A tiny monitor must not fall below the floor.
        assert_eq!(starting_bitrate(640, 480, 30, bounds), bounds.min_kbps);
        // A huge one must not exceed the ceiling.
        assert_eq!(starting_bitrate(3840, 2160, 60, bounds), bounds.max_kbps);
    }

    #[test]
    fn monotonic_clock_moves_forward_only() {
        let a = monotonic_us();
        std::thread::sleep(Duration::from_millis(2));
        assert!(monotonic_us() > a);
    }
}
