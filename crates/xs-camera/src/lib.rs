//! Feeding the tablet's camera into a v4l2loopback device.
//!
//! The tablet sends H.264, which is decoded here and written as raw frames to
//! `/dev/video10`, where every ordinary camera consumer -- Firefox, Zoom, OBS,
//! Cheese -- picks it up as a normal webcam.
//!
//! ```text
//! appsrc -> h264parse -> avdec_h264 -> videoconvert -> videoscale -> v4l2sink
//! ```
//!
//! Decoding rather than passing H.264 through is deliberate: v4l2loopback can
//! carry encoded formats, but almost nothing consuming a webcam expects them, and
//! the whole point is to look like an ordinary camera.

use std::path::{Path, PathBuf};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSrc;
use tracing::{debug, info, warn};

/// Where `scripts/setup.sh` puts the loopback device.
pub const DEFAULT_DEVICE: &str = "/dev/video10";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("GStreamer init failed: {0}")]
    Init(#[from] gst::glib::Error),

    #[error(
        "{path} does not exist. Run ./scripts/setup.sh to create the virtual \
         camera device, then try again."
    )]
    DeviceMissing { path: PathBuf },

    #[error(
        "{path} exists but could not be opened for writing. Is another program \
         already feeding it, or is your user missing from the 'video' group?"
    )]
    DeviceBusy { path: PathBuf },

    #[error("could not build the '{element}' element -- is its GStreamer plugin installed?")]
    ElementMissing { element: &'static str },

    #[error("pipeline error: {0}")]
    Pipeline(String),

    #[error("failed to link the pipeline: {0}")]
    Link(#[from] gst::glib::BoolError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decodes incoming H.264 and writes it to a v4l2loopback device.
pub struct V4l2Writer {
    pipeline: gst::Pipeline,
    source: AppSrc,
    started: bool,
    device: PathBuf,
}

impl V4l2Writer {
    /// Opens [`DEFAULT_DEVICE`].
    pub fn open_default() -> Result<Self> {
        Self::open(Path::new(DEFAULT_DEVICE))
    }

    pub fn open(device: &Path) -> Result<Self> {
        gst::init()?;

        if !device.exists() {
            return Err(Error::DeviceMissing {
                path: device.to_path_buf(),
            });
        }
        // Fail here, with a message that says what to do, rather than letting
        // GStreamer report a generic state-change failure later.
        match std::fs::OpenOptions::new().write(true).open(device) {
            Ok(f) => drop(f),
            Err(e) => {
                debug!(error = %e, "probing the loopback device failed");
                return Err(Error::DeviceBusy {
                    path: device.to_path_buf(),
                });
            }
        }

        let pipeline = gst::Pipeline::with_name("extraspace-camera");
        let make = |name: &'static str| -> Result<gst::Element> {
            gst::ElementFactory::make(name)
                .build()
                .map_err(|_| Error::ElementMissing { element: name })
        };

        // The tablet sends Annex-B with inline SPS/PPS, and we cannot know the
        // resolution until the stream arrives, so leave caps for h264parse to
        // work out rather than asserting something that might be wrong.
        let source = AppSrc::builder()
            .name("camera-in")
            .caps(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .format(gst::Format::Time)
            .is_live(true)
            // Drop rather than block if the decoder falls behind: a stale webcam
            // frame is worse than a missing one.
            .max_bytes(4 * 1024 * 1024)
            .build();

        let parse = make("h264parse")?;
        let decode = make("avdec_h264")?;
        let convert = make("videoconvert")?;
        let scale = make("videoscale")?;

        // YUY2 is the format essentially every v4l2 consumer understands.
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw").field("format", "YUY2").build(),
            )
            .build()
            .map_err(|_| Error::ElementMissing { element: "capsfilter" })?;

        let sink = gst::ElementFactory::make("v4l2sink")
            .property("device", device.to_string_lossy().as_ref())
            // The loopback device has no clock of its own to sync against.
            .property("sync", false)
            .build()
            .map_err(|_| Error::ElementMissing { element: "v4l2sink" })?;

        let elements = [
            source.upcast_ref(),
            &parse,
            &decode,
            &convert,
            &scale,
            &caps,
            &sink,
        ];
        pipeline.add_many(elements)?;
        gst::Element::link_many(elements)?;

        info!(device = %device.display(), "virtual camera ready");
        Ok(Self {
            pipeline,
            source,
            started: false,
            device: device.to_path_buf(),
        })
    }

    /// Pushes one encoded access unit.
    ///
    /// The pipeline starts lazily on the first frame: starting at construction
    /// would have `v4l2sink` announce a camera that then shows nothing, and some
    /// consumers latch onto the format they see first.
    pub fn push(&mut self, data: &[u8], pts_us: u64, _is_config: bool) -> Result<()> {
        if !self.started {
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| Error::Pipeline(e.to_string()))?;
            self.watch_bus();
            self.started = true;
            debug!("camera pipeline playing");
        }

        let mut buffer = gst::Buffer::from_slice(data.to_vec());
        {
            let buffer = buffer.get_mut().expect("freshly created buffer is unique");
            buffer.set_pts(gst::ClockTime::from_useconds(pts_us));
        }

        match self.source.push_buffer(buffer) {
            Ok(_) => Ok(()),
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => {
                Err(Error::Pipeline("camera pipeline stopped".into()))
            }
            Err(e) => Err(Error::Pipeline(format!("{e:?}"))),
        }
    }

    pub fn device(&self) -> &Path {
        &self.device
    }

    fn watch_bus(&self) {
        let Some(bus) = self.pipeline.bus() else { return };
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        warn!(error = %e.error(), debug = ?e.debug(), "camera pipeline error");
                        break;
                    }
                    gst::MessageView::Eos(_) => break,
                    _ => {}
                }
            }
        });
    }
}

impl Drop for V4l2Writer {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            debug!(error = %e, "camera pipeline did not stop cleanly");
        }
    }
}

/// Whether a loopback device is present, for the UI to show a setup hint.
pub fn device_available() -> bool {
    Path::new(DEFAULT_DEVICE).exists()
}
