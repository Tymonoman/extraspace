//! Extraspace: use an Android tablet as an extra display and webcam, over USB.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

mod config;
mod window;

use config::Config;
use window::APP_ID;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,xs_core=debug,xs_mutter=debug".into()),
        )
        .init();

    // Fail here with something readable rather than deep inside the pipeline.
    if let Err(e) = gstreamer::init() {
        eprintln!("Could not initialise GStreamer: {e}");
        eprintln!("Try running ./scripts/setup.sh");
        return glib::ExitCode::FAILURE;
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        let config = Rc::new(RefCell::new(Config::load()));
        let engine = xs_core::spawn(session_config(&config.borrow()));
        window::build(app, engine, config);
    });

    app.run()
}

fn session_config(config: &Config) -> xs_core::SessionConfig {
    xs_core::SessionConfig {
        mode: config.display_mode(),
        scale: config.scale,
        framerate: config.framerate,
        bounds: config.bounds(),
        mirror_source: config.mirror_source.clone(),
        apk_path: bundled_apk(),
        apk_version: APK_VERSION,
        camera_enabled: config.camera_enabled,
        camera_id: config.camera_id.clone(),
    }
}

/// Must match `versionCode` in `android/app/build.gradle.kts`; the host pushes a
/// new APK whenever the tablet has an older one.
const APK_VERSION: u32 = 1;

/// Finds the companion APK.
///
/// Checked in order of how likely each is to be the one you want: an explicit
/// override, then a local build, then an installed copy. Returning `None` is
/// fine -- the host then assumes the app is already installed.
fn bundled_apk() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("EXTRASPACE_APK") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }

    let candidates = [
        // A locally built APK, so `cargo run` after a Gradle build picks up your
        // changes with no extra steps.
        PathBuf::from("android/app/build/outputs/apk/release/app-release.apk"),
        PathBuf::from("android/app/build/outputs/apk/debug/app-debug.apk"),
        PathBuf::from("/usr/share/extraspace/extraspace.apk"),
        PathBuf::from("/usr/local/share/extraspace/extraspace.apk"),
    ];
    candidates.into_iter().find(|p| p.exists())
}
