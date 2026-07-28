//! Capture from a mutter screen-cast PipeWire node, encode to H.264.
//!
//! The pipeline is deliberately boring:
//!
//! ```text
//! pipewiresrc -> videorate -> videoconvert -> <encoder> -> h264parse -> appsink
//! ```
//!
//! `videorate` caps the stream at the configured rate; it does **not** pad it up
//! to that rate. Mutter only emits a frame when something on the monitor actually
//! changes, so a still desktop measures around 11 fps rather than 60. That is
//! correct and desirable -- an idle screen costs almost no bandwidth -- but it
//! means frame-count-based reasoning is unreliable: see the keyframe note below.
//!
//! Encoded frames leave through a bounded channel. If the transport cannot keep
//! up, frames are dropped here rather than allowed to accumulate -- for a live
//! display, a stale frame has no value.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSink;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

mod encoder;
pub use encoder::Encoder;

/// Encoded frames buffered before we start dropping. Small on purpose: latency
/// matters far more than completeness for a live display.
const FRAME_QUEUE_DEPTH: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("GStreamer init failed: {0}")]
    Init(#[from] gst::glib::Error),

    #[error(
        "no usable H.264 encoder found. Install gstreamer1-plugins-ugly (for x264enc) \
         or gstreamer1-plugin-openh264."
    )]
    NoEncoder,

    #[error("could not build the '{element}' element -- is its GStreamer plugin installed?")]
    ElementMissing { element: &'static str },

    #[error("GStreamer pipeline error: {0}")]
    Pipeline(String),

    #[error("failed to link the pipeline: {0}")]
    Link(#[from] gst::glib::BoolError),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_kbps: 15_000,
        }
    }
}

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub pts_us: u64,
    pub keyframe: bool,
}

/// Counters for the UI's statistics panel and the adaptive controller.
#[derive(Debug, Default)]
pub struct VideoStats {
    pub frames_encoded: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub bytes_encoded: AtomicU64,
}

impl VideoStats {
    /// `(encoded, dropped, bytes)`
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.frames_encoded.load(Ordering::Relaxed),
            self.frames_dropped.load(Ordering::Relaxed),
            self.bytes_encoded.load(Ordering::Relaxed),
        )
    }
}

pub struct VideoPipeline {
    pipeline: gst::Pipeline,
    encoder_element: gst::Element,
    encoder: Encoder,
    stats: Arc<VideoStats>,
    config: VideoConfig,
}

impl VideoPipeline {
    /// Builds the pipeline for a mutter PipeWire node. Call [`start`](Self::start)
    /// to begin producing frames.
    pub fn new(node_id: u32, config: VideoConfig) -> Result<(Self, mpsc::Receiver<EncodedFrame>)> {
        gst::init()?;

        let encoder = Encoder::detect().ok_or(Error::NoEncoder)?;
        let stats = Arc::new(VideoStats::default());
        let pipeline = gst::Pipeline::with_name("extraspace-display");

        let make = |name: &'static str| -> Result<gst::Element> {
            gst::ElementFactory::make(name)
                .build()
                .map_err(|_| Error::ElementMissing { element: name })
        };
        let caps_filter = |caps: gst::Caps| -> Result<gst::Element> {
            gst::ElementFactory::make("capsfilter")
                .property("caps", caps)
                .build()
                .map_err(|_| Error::ElementMissing {
                    element: "capsfilter",
                })
        };

        let src = gst::ElementFactory::make("pipewiresrc")
            .property("path", node_id.to_string())
            // Screen-cast buffers do not carry timestamps useful to us.
            .property("do-timestamp", true)
            .build()
            .map_err(|_| Error::ElementMissing {
                element: "pipewiresrc",
            })?;

        let rate = make("videorate")?;

        let rate_caps = caps_filter(
            gst::Caps::builder("video/x-raw")
                .field("framerate", gst::Fraction::new(config.framerate as i32, 1))
                .build(),
        )?;

        let convert = make("videoconvert")?;
        let convert_caps = caps_filter(
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .build(),
        )?;

        let encoder_element = encoder.build(config.bitrate_kbps, config.framerate)?;

        let parse = gst::ElementFactory::make("h264parse")
            // Repeat SPS/PPS on every keyframe: the tablet may attach late or
            // reconnect, and without inline headers its decoder cannot start.
            .property("config-interval", -1i32)
            .build()
            .map_err(|_| Error::ElementMissing {
                element: "h264parse",
            })?;

        let parse_caps = caps_filter(
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        )?;

        let appsink = AppSink::builder()
            .name("frames")
            .max_buffers(FRAME_QUEUE_DEPTH as u32)
            .drop(true)
            // Push frames out as fast as they are encoded; do not pace to the clock.
            .sync(false)
            .build();

        let elements = [
            &src,
            &rate,
            &rate_caps,
            &convert,
            &convert_caps,
            &encoder_element,
            &parse,
            &parse_caps,
            appsink.upcast_ref(),
        ];
        pipeline.add_many(elements)?;
        gst::Element::link_many(elements)?;

        let (tx, rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
        let sink_stats = Arc::clone(&stats);

        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);
                    let frame = EncodedFrame {
                        data: Bytes::copy_from_slice(map.as_slice()),
                        pts_us,
                        keyframe,
                    };

                    sink_stats
                        .bytes_encoded
                        .fetch_add(map.len() as u64, Ordering::Relaxed);
                    match tx.try_send(frame) {
                        Ok(()) => {
                            sink_stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Transport is behind. Dropping now keeps latency bounded;
                            // the adaptive controller sees this and lowers bitrate.
                            sink_stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err(gst::FlowError::Eos);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        info!(
            node_id,
            encoder = encoder.element_name(),
            width = config.width,
            height = config.height,
            framerate = config.framerate,
            bitrate_kbps = config.bitrate_kbps,
            "display pipeline built"
        );

        Ok((
            Self {
                pipeline,
                encoder_element,
                encoder,
                stats,
                config,
            },
            rx,
        ))
    }

    pub fn start(&self) -> Result<()> {
        self.watch_bus();
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Pipeline(e.to_string()))?;
        Ok(())
    }

    pub fn stop(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            warn!(error = %e, "pipeline did not stop cleanly");
        }
    }

    /// Adjusts bitrate on the running pipeline; the adaptive controller's lever.
    pub fn set_bitrate(&self, kbps: u32) {
        self.encoder.set_bitrate(&self.encoder_element, kbps);
        debug!(kbps, "bitrate updated");
    }

    /// Asks the encoder for an immediate keyframe, with headers. Used when the
    /// tablet reconnects or reports it cannot decode.
    pub fn request_keyframe(&self) {
        let structure = gst::Structure::builder("GstForceKeyUnit")
            .field("all-headers", true)
            .build();
        if !self
            .encoder_element
            .send_event(gst::event::CustomUpstream::new(structure))
        {
            debug!("encoder did not accept the force-keyframe request");
        }
    }

    pub fn stats(&self) -> &Arc<VideoStats> {
        &self.stats
    }

    pub fn encoder(&self) -> Encoder {
        self.encoder
    }

    pub fn config(&self) -> &VideoConfig {
        &self.config
    }

    /// Logs asynchronous pipeline errors, which otherwise surface only as a
    /// silently stalled stream.
    fn watch_bus(&self) {
        let Some(bus) = self.pipeline.bus() else {
            return;
        };
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        error!(
                            source = ?e.src().map(|s| s.path_string()),
                            error = %e.error(),
                            debug = ?e.debug(),
                            "pipeline error"
                        );
                        break;
                    }
                    gst::MessageView::Warning(w) => {
                        warn!(warning = %w.error(), "pipeline warning");
                    }
                    gst::MessageView::Eos(_) => {
                        info!("pipeline reached end of stream");
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}
