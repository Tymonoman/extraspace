//! Injects a keyboard shortcut into the running GNOME session.
//!
//! Useful on its own for testing the keyboard path, and used to stage the demo
//! recording: `Super+Shift+Right` moves the focused window onto the next
//! monitor, which is how you get a window onto the tablet without touching it.
//!
//! Keyboard events are *global* rather than stream-relative, so this works from
//! a bare remote-desktop session with no screen cast attached -- which means it
//! can run alongside Extraspace rather than fighting it for a virtual monitor.
//!
//! ```console
//! cargo run -p xs-mutter --example send_keys -- super+shift+right
//! ```
//!
//! Keycodes are Linux evdev codes, not X11 keycodes (which are offset by 8).

use std::time::Duration;

use xs_mutter::keys;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let combo = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "super+shift+right".into());

    let Some(chord) = keys::parse_chord(&combo) else {
        eprintln!("could not parse '{combo}'");
        eprintln!("expected something like: super+shift+right, ctrl+alt+t, super");
        std::process::exit(1);
    };

    let session = xs_mutter::InputOnlySession::open().await?;
    // Give the compositor a moment to settle before the first event, otherwise
    // the leading modifier is occasionally swallowed.
    tokio::time::sleep(Duration::from_millis(150)).await;
    session.send_chord(&chord).await?;
    println!("sent {combo}");

    // Let the events drain before the session drops.
    tokio::time::sleep(Duration::from_millis(150)).await;
    session.close().await?;
    Ok(())
}
