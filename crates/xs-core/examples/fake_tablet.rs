//! Runs the entire host pipeline against a simulated tablet, on one machine.
//!
//! Stands in for everything the real device would do -- listens on the three
//! sockets, introduces itself, consumes the video stream, answers pings and
//! reports decode statistics -- so the host half can be exercised with no
//! Android hardware, no adb and no APK.
//!
//! ```console
//! cargo run -p xs-core --example fake_tablet
//! ```
//!
//! What this proves: session orchestration, the `Hello`/`VideoConfig` handshake,
//! virtual monitor creation, capture, encoding, framing over real sockets, touch
//! injection into the compositor, and the adaptive controller reacting to health
//! reports. What it cannot prove: MediaCodec decoding and anything USB-specific.

use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use xs_core::{Command, Event, SessionConfig, State};
use xs_proto::{ports, Channel, ControlKind, Hello, TouchAction, TouchEvent};
use xs_transport::{FrameReader, FrameWriter};

/// Pretend to be the T Tablet this was developed against.
const PANEL_WIDTH: u32 = 2000;
const PANEL_HEIGHT: u32 = 1200;
const RUN_FOR: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xs_core=debug".into()),
        )
        .init();

    // Listen before the engine is told to connect, since the host is the client.
    let control = TcpListener::bind(("127.0.0.1", ports::CONTROL)).await?;
    let video = TcpListener::bind(("127.0.0.1", ports::VIDEO)).await?;
    let camera = TcpListener::bind(("127.0.0.1", ports::CAMERA)).await?;
    println!("simulated tablet listening on {}/{}/{}", ports::CONTROL, ports::VIDEO, ports::CAMERA);

    std::env::set_var(xs_transport::FAKE_TABLET_ENV, "1");
    let engine = xs_core::spawn(SessionConfig::default());

    let mut events = engine.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                Event::State(State::Connecting { step }) => println!("  [host] {step}"),
                Event::State(State::Streaming { width, height, encoder, .. }) => {
                    println!("  [host] streaming {width}x{height} via {encoder}");
                }
                Event::State(State::Failed { message }) => println!("  [host] FAILED: {message}"),
                Event::Warning(w) => println!("  [host] warning: {w}"),
                _ => {}
            }
        }
    });

    engine.send(Command::Connect);

    // --- control channel: handshake, pings, stats -------------------------
    let (control_sock, _) = control.accept().await?;
    control_sock.set_nodelay(true)?;
    let (control_rx, control_tx) = control_sock.into_split();
    let mut control_reader = FrameReader::new(control_rx);
    let mut control_writer = FrameWriter::new(control_tx);

    let hello = Hello {
        protocol_version: xs_proto::PROTOCOL_VERSION,
        device_name: "Simulated T Tablet".into(),
        android_release: "15".into(),
        width: PANEL_WIDTH,
        height: PANEL_HEIGHT,
        density_dpi: 240,
        refresh_rate: 60.0,
        cameras: vec![],
    };
    control_writer
        .write_frame(
            Channel::Control,
            ControlKind::Hello as u8,
            0,
            0,
            serde_json::to_string(&hello)?.as_bytes(),
        )
        .await?;
    println!("  [tablet] sent Hello ({PANEL_WIDTH}x{PANEL_HEIGHT})");

    // --- video channel: consume the stream --------------------------------
    let (video_sock, _) = video.accept().await?;
    let (video_rx, _video_tx) = video_sock.into_split();
    let mut video_reader = FrameReader::new(video_rx);
    let _ = camera.accept().await; // accepted so the host's connect completes

    let stats = std::sync::Arc::new(std::sync::Mutex::new(Counters::default()));
    let video_stats = std::sync::Arc::clone(&stats);
    tokio::spawn(async move {
        while let Ok(frame) = video_reader.read_frame().await {
            let mut c = video_stats.lock().unwrap();
            c.frames += 1;
            c.bytes += frame.payload.len() as u64;
            if frame.header.flags & xs_proto::flags::KEYFRAME != 0 {
                c.keyframes += 1;
            }
            if c.first_frame_at.is_none() {
                c.first_frame_at = Some(Instant::now());
            }
        }
    });

    let started = Instant::now();
    let mut stats_ticker = tokio::time::interval(Duration::from_millis(500));
    let mut touches_sent = 0u32;
    let mut pongs = 0u32;

    loop {
        if started.elapsed() >= RUN_FOR {
            break;
        }
        tokio::select! {
            // Answer whatever the host sends us.
            frame = control_reader.read_frame() => {
                let Ok(frame) = frame else { break };
                if frame.header.kind == ControlKind::Ping as u8 {
                    control_writer
                        .write_frame(Channel::Control, ControlKind::Pong as u8, 0, frame.header.pts_us, &[])
                        .await?;
                    pongs += 1;
                }
            }

            _ = stats_ticker.tick() => {
                let snapshot = { let c = stats.lock().unwrap(); (c.frames, c.bytes) };
                let device_stats = xs_proto::Stats {
                    // Report a healthy decoder so the controller is free to probe
                    // upwards -- that path is otherwise never taken.
                    decode_queue_depth: 0,
                    frames_decoded: snapshot.0,
                    frames_dropped: 0,
                    last_frame_pts_us: 0,
                    rendered_at_us: 0,
                };
                control_writer
                    .write_frame(
                        Channel::Control, ControlKind::Stats as u8, 0, 0,
                        serde_json::to_string(&device_stats)?.as_bytes(),
                    )
                    .await?;

                // Drag a finger across the virtual monitor. These become real
                // NotifyTouch* calls into mutter.
                let t = started.elapsed().as_secs_f64();
                let x = 100.0 + (t * 80.0) % 600.0;
                let action = if touches_sent == 0 { TouchAction::Down } else { TouchAction::Motion };
                let touch = TouchEvent { action, slot: 0, x, y: 200.0 };
                control_writer
                    .write_frame(Channel::Touch, 0, 0, 0, &touch.encode())
                    .await?;
                touches_sent += 1;
            }
        }
    }

    control_writer
        .write_frame(
            Channel::Touch, 0, 0, 0,
            &TouchEvent { action: TouchAction::Up, slot: 0, x: 0.0, y: 0.0 }.encode(),
        )
        .await?;
    control_writer.flush().await?;

    engine.send(Command::Shutdown);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let c = stats.lock().unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    println!("\n--- simulated tablet results ---");
    match c.first_frame_at {
        Some(t) => println!("  first video frame   {:.0} ms after start", t.duration_since(started).as_secs_f64() * 1000.0),
        None => println!("  first video frame   NEVER ARRIVED"),
    }
    println!("  video frames        {}", c.frames);
    println!("  keyframes           {}", c.keyframes);
    println!("  bytes received      {}", c.bytes);
    println!("  effective bitrate   {:.2} Mbps", c.bytes as f64 * 8.0 / 1_000_000.0 / elapsed);
    println!("  pings answered      {pongs}");
    println!("  touch events sent   {touches_sent}");

    anyhow::ensure!(c.frames > 0, "no video ever reached the tablet");
    anyhow::ensure!(c.keyframes > 0, "no keyframe: a real tablet could not have started decoding");
    anyhow::ensure!(pongs > 0, "host never pinged: latency measurement is dead");
    println!("\n  PASS");
    Ok(())
}

#[derive(Default)]
struct Counters {
    frames: u64,
    keyframes: u64,
    bytes: u64,
    first_frame_at: Option<Instant>,
}
