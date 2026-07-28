//! Creates a virtual monitor, holds it briefly, then removes it.
//!
//! This is the Rust port of the Phase 0 feasibility spike and doubles as a quick
//! way to check the mutter side in isolation:
//!
//! ```console
//! cargo run -p xs-mutter --example virtual_monitor
//! ```
//!
//! Expect a new display to appear in Settings -> Displays for a few seconds.
//! Windows may briefly rearrange; everything reverts on exit.

use std::time::Duration;

use xs_mutter::{CaptureSource, CursorMode, DisplayConfig, Session};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xs_mutter=debug".into()),
        )
        .init();

    println!("monitors before: {:?}", xs_mutter::list_monitors().await?);

    let session = Session::open(DisplayConfig {
        width: 2000,
        height: 1200,
        refresh_rate: 60.0,
        cursor_mode: CursorMode::Embedded,
        source: CaptureSource::Virtual,
    })
    .await?;

    println!("\n  pipewire node id : {}", session.node_id());
    println!(
        "  consume it with  : gst-launch-1.0 pipewiresrc path={} ! videoconvert ! autovideosink\n",
        session.node_id()
    );
    println!("monitors during: {:?}", xs_mutter::list_monitors().await?);

    // Prove input injection reaches the compositor: trace a short diagonal on the
    // new monitor. Nothing is there to click, so this is safe.
    println!("\ninjecting a touch stroke on the virtual monitor...");
    session.touch_down(0, 100.0, 100.0).await?;
    for i in 1..=10 {
        tokio::time::sleep(Duration::from_millis(16)).await;
        session
            .touch_motion(0, 100.0 + i as f64 * 20.0, 100.0 + i as f64 * 12.0)
            .await?;
    }
    session.touch_up(0).await?;
    println!("touch stroke delivered without error");

    println!("\nholding for 5s...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    session.close().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("monitors after: {:?}", xs_mutter::list_monitors().await?);
    Ok(())
}
