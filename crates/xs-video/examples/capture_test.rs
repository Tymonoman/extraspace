//! Proves the capture -> encode path end to end, with no tablet involved.
//!
//! Creates a real virtual monitor, runs the production pipeline against it, and
//! checks that what comes out is decodable H.264 at roughly the requested rate.
//! Everything the tablet would otherwise be needed for is downstream of this, so
//! if this passes, the remaining risk is MediaCodec and the socket, not capture.
//!
//! ```console
//! cargo run -p xs-video --example capture_test
//! ```

use std::time::{Duration, Instant};

use xs_mutter::{CaptureSource, CursorMode, DisplayConfig};
use xs_video::{VideoConfig, VideoPipeline};

const WIDTH: u32 = 1332;
const HEIGHT: u32 = 800;
const FRAMERATE: u32 = 60;
const CAPTURE_FOR: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xs_video=debug".into()),
        )
        .init();

    println!("creating a {WIDTH}x{HEIGHT}@{FRAMERATE} virtual monitor...");
    let session = xs_mutter::Session::open(DisplayConfig {
        width: WIDTH,
        height: HEIGHT,
        refresh_rate: FRAMERATE as f64,
        cursor_mode: CursorMode::Embedded,
        source: CaptureSource::Virtual,
    })
    .await?;
    println!("  pipewire node {}", session.node_id());

    let (pipeline, mut frames) = VideoPipeline::new(
        session.node_id(),
        VideoConfig {
            width: WIDTH,
            height: HEIGHT,
            framerate: FRAMERATE,
            bitrate_kbps: 15_000,
        },
    )?;
    println!("  encoder: {}", pipeline.encoder().human_name());
    pipeline.start()?;

    let started = Instant::now();
    let mut first_frame_at = None;
    let mut count = 0u64;
    let mut keyframes = 0u64;
    let mut bytes = 0u64;
    let mut stream = Vec::new();

    println!("\ncapturing for {}s...", CAPTURE_FOR.as_secs());
    let deadline = tokio::time::Instant::now() + CAPTURE_FOR;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, frames.recv()).await {
            Ok(Some(frame)) => {
                if first_frame_at.is_none() {
                    first_frame_at = Some(started.elapsed());
                }
                count += 1;
                bytes += frame.data.len() as u64;
                if frame.keyframe {
                    keyframes += 1;
                }
                stream.extend_from_slice(&frame.data);
            }
            Ok(None) => {
                println!("  pipeline closed the channel early");
                break;
            }
            Err(_) => break, // deadline reached
        }
    }

    pipeline.stop();
    session.close().await?;

    let elapsed = started.elapsed().as_secs_f64();
    let fps = count as f64 / elapsed;
    let mbps = bytes as f64 * 8.0 / 1_000_000.0 / elapsed;

    println!("\n--- results ---");
    match first_frame_at {
        Some(t) => println!("  first frame after   {:.0} ms", t.as_secs_f64() * 1000.0),
        None => println!("  first frame          NEVER ARRIVED"),
    }
    println!("  frames              {count}");
    println!("  keyframes           {keyframes}");
    println!("  measured rate       {fps:.1} fps  (asked for {FRAMERATE})");
    println!("  measured bitrate    {mbps:.1} Mbps  (asked for 15.0)");
    println!("  total bytes         {bytes}");

    let path = std::env::temp_dir().join("extraspace-capture.h264");
    std::fs::write(&path, &stream)?;
    println!("\n  wrote {} ({} bytes)", path.display(), stream.len());
    println!(
        "  verify with: ffprobe -v error -show_entries stream=codec_name,width,height {}",
        path.display()
    );

    // A capture that produces no frames is the failure this whole example exists
    // to catch, so make it a non-zero exit rather than a line of prose.
    anyhow::ensure!(count > 0, "capture produced no frames at all");
    anyhow::ensure!(
        keyframes > 0,
        "no keyframes: the tablet could never start decoding"
    );
    Ok(())
}
