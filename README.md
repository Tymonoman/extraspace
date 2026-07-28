<div align="center">

# Extraspace

**Turn an Android tablet into a real second monitor for GNOME — over USB, with touch.**

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![GNOME](https://img.shields.io/badge/GNOME-46%2B-4A86CF.svg)](https://www.gnome.org)
[![Wayland](https://img.shields.io/badge/Wayland-native-green.svg)](https://wayland.freedesktop.org)

</div>

---

Extraspace makes your Android tablet appear in **Settings → Displays** as a genuine
monitor. Not a screen-share window, not a VNC session — a real output you can drag
windows onto, arrange, and give its own workspaces. Touch the tablet and it drives
the cursor there.

It also sends the tablet's camera back the other way, exposing it to Linux as an
ordinary webcam that Firefox, Zoom, OBS and Cheese pick up automatically.

Everything runs over the **USB cable**. No network, no cloud, no account.

```
┌──────────────────────────┐         ┌────────────────────────┐
│  GNOME / Wayland         │   USB   │  Android tablet        │
│                          │◄───────►│                        │
│  ┌────────┐  ┌────────┐  │         │   ┌────────────────┐   │
│  │  DP-3  │  │ HDMI-1 │  │         │   │   Meta-0       │   │
│  └────────┘  └────────┘  │         │   │  "Virtual      │   │
│  ┌────────────────────┐  │         │   │   remote       │   │
│  │      Meta-0        │──┼─────────┼──►│   monitor"     │   │
│  │  (the tablet)      │◄─┼─ touch ─┼───│                │   │
│  └────────────────────┘  │         │   └────────────────┘   │
│  /dev/video10 ◄──────────┼── cam ──┼───  camera             │
└──────────────────────────┘         └────────────────────────┘
```

## Why this exists

Most "tablet as second screen" tools on Linux mirror an existing display, or need
X11, or route video over Wi-Fi with the latency that implies. On GNOME Wayland
specifically, creating a *new* output has historically meant DisplayLink drivers
or the EVDI kernel module.

It turns out mutter can already do it. `org.gnome.Mutter.ScreenCast` has a
`RecordVirtual` method that creates a monitor with no backing hardware, and
`org.gnome.Mutter.RemoteDesktop` can inject touch events whose coordinates are
*relative to that stream* — so input lands on the right monitor with no geometry
maths at all. Extraspace is a well-behaved GNOME app wrapped around those two APIs.

## Status

Honest state of things, so nobody wastes an evening:

| Piece | State |
|---|---|
| Virtual monitor creation, teardown | **Working** — verified on GNOME 50.3 / mutter 50.3 |
| Touch injection into the virtual monitor | **Working** — verified end to end |
| Capture → H.264 encode | **Working** — captures the real desktop, output decodes cleanly |
| Wire protocol, adb transport | **Working** — unit tested |
| Adaptive bitrate control | **Working** — unit tested |
| Tablet-side decode and display | **Written, not yet run on a tablet** |
| Camera → `/dev/video10` | **Written, not yet run on a tablet** |
| Keyboard, stylus, audio, Wi-Fi | Not started — see [Roadmap](#roadmap) |

You can verify the capture half yourself, with no tablet plugged in:

```console
$ cargo run -p xs-video --example capture_test
creating a 1332x800@60 virtual monitor...
  pipewire node 92
  encoder: OpenH264 (software, fallback)

capturing for 5s...
--- results ---
  first frame after   46 ms
  frames              57
  keyframes           3
  measured rate       11.4 fps  (asked for 60)
```

It writes `/tmp/extraspace-capture.h264`, which `ffprobe` will confirm is
Constrained Baseline 1332×800 that decodes without errors.

If you try it and something breaks, an issue with `RUST_LOG=debug` output is very
welcome.

## Requirements

- **GNOME 46 or newer on Wayland.** This is not portable to other compositors:
  it depends on mutter-specific D-Bus APIs that KDE, Sway and friends do not have.
  GNOME 50+ additionally lets the monitor be pinned to an exact mode.
- An **Android 11+** tablet (API 30, for `MediaCodec` low-latency decoding).
- A **USB cable** and USB debugging enabled on the tablet.
- Any GPU. Encoding is done on the CPU by default and needs roughly one core.

## Install

```bash
git clone https://github.com/Tymonoman/extraspace
cd extraspace
./scripts/setup.sh      # installs dependencies, creates /dev/video10
cargo build --release
./target/release/extraspace
```

`setup.sh` is the only step that needs `sudo`. It installs the GStreamer and GTK
development packages, sets up the virtual camera device so it survives reboots,
and tells you whether your tablet is visible. Run it with `--check` to see what it
would do without changing anything.

You will also need the companion app on the tablet. Grab `extraspace.apk` from the
[latest release](https://github.com/Tymonoman/extraspace/releases) and either
sideload it, or just let Extraspace push it for you:

```bash
EXTRASPACE_APK=~/Downloads/extraspace.apk cargo run --release
```

Once an APK is on the tablet, the host checks its version on every connect and
upgrades it automatically, so the two halves can never drift apart.

### Enabling USB debugging

On the tablet:

1. **Settings → About tablet** → tap **Build number** seven times.
2. **Settings → System → Developer options** → turn on **USB debugging**.
3. Plug in the USB cable, then accept the *Allow USB debugging?* prompt.
   Tick **Always allow from this computer**.

If you skip step 3 the app will tell you so explicitly rather than failing with
something cryptic — it is by far the most common first-run problem.

## Usage

Open Extraspace and turn on **Extra Display**. That is the whole workflow.

| Setting | What it does |
|---|---|
| **Mode** | *Extend* adds a new monitor. *Mirror* copies an existing one. |
| **Scale** | How large the desktop is drawn on the tablet. See below. |
| **Tablet Camera** | Feeds the tablet camera into `/dev/video10`. |

### About scale

A 10.4" tablet at its native 2000×1200 renders GNOME at a size that is technically
correct and practically unreadable. Extraspace handles this by creating the
virtual monitor *smaller* than the panel and letting the tablet upscale:

| Scale | Monitor created | Result |
|---|---|---|
| 1× | 2000 × 1200 | Pin-sharp, very small text |
| **1.5×** (default) | 1332 × 800 | Comfortable — the sweet spot |
| 2× | 1000 × 600 | Large text, noticeably soft |

Higher scale also costs less bandwidth, because there are fewer pixels to encode.

## How it works

The interesting part is the sequence, which is not obvious from mutter's interface
XML and took some experimentation to get right:

1. `RemoteDesktop.CreateSession()` → read its `SessionId`.
2. `ScreenCast.CreateSession({"remote-desktop-session-id": id})`. **Linking the two
   sessions is what makes injected input land on the virtual monitor.**
3. `ScreenCast.Session.RecordVirtual({ modes, is-platform, cursor-mode })`.
   - `is-platform: true` makes it a real monitor rather than a shared surface.
   - `modes` pins it to an exact resolution so PipeWire cannot renegotiate it.
4. Subscribe to `PipeWireStreamAdded` **before** starting, or the signal is missed.
5. `RemoteDesktop.Session.Start()` — *not* `ScreenCast.Session.Start()`, which
   mutter rejects for a linked session with *"Must be started from remote desktop
   session"*. Teardown is symmetric.

From there it is a normal GStreamer pipeline:

```
pipewiresrc → videorate → videoconvert → x264enc → h264parse → appsink → USB
```

and on the tablet, `MediaCodec` → `SurfaceView`. Touches travel back on a separate
socket and become `NotifyTouchDown/Motion/Up` calls, whose coordinates are already
in the virtual monitor's space.

### Notes from building it

Things that cost time, recorded so they cost you less:

- **`vulkanh264enc` silently ignores its bitrate setting.** It advertises CBR and
  accepts the property, but produces byte-identical output at 5, 15 and 40 Mbit/s.
  Unusable for adaptive streaming. Extraspace does not offer it.
- **Fedora strips NVENC out of GStreamer's `nvcodec` plugin.** Only the CUDA
  utility elements register; `nvh264enc` does not exist, and `plugins-freeworld`
  does not add it. `x264enc` at `veryfast` manages ~168 fps at 2000×1200 on a
  mid-range CPU, which is 2.8× more than needed, so this matters less than it sounds.
- **`x264enc` takes kbit/s but `openh264enc` takes bit/s.** A 1000× error waiting
  to happen; the conversion lives in exactly one function.
- **USB 2.0 is not the bottleneck.** Raw 2000×1200@60 would need ~550 MB/s, far
  beyond the ~30 MB/s a High Speed link gives you. Encoded H.264 at 15 Mbit/s is
  under 2 MB/s — roughly 15× headroom.
- **"60 fps" is a ceiling, not a rate.** Mutter only emits a frame when something
  on the monitor actually changes, so a still desktop measures around **11 fps**
  and well under 1 Mbit/s. That is exactly what you want — an idle screen should
  be nearly free — but it quietly breaks anything that reasons in frame counts.
  Both encoders express their keyframe interval in frames, so the obvious
  `framerate × 2` puts keyframes *ten seconds* apart while idle, and a tablet
  that reconnects sits on a black screen until one arrives. The interval is sized
  against the idle rate instead, and a keyframe is requested explicitly whenever a
  tablet attaches.

## Troubleshooting

**"No tablet found"** — check `adb devices`. If it is empty, the cable may be
charge-only; many are. If it says `unauthorized`, accept the prompt on the tablet.

**"Something went wrong: no usable H.264 encoder"** — run
`./scripts/setup.sh`, or install `gstreamer1-plugins-ugly` (x264) or
`gstreamer1-plugin-openh264`.

**The virtual camera does not appear** — run `./scripts/setup.sh` to create
`/dev/video10`. Note that `exclusive_caps=1` deliberately hides it from
applications while nothing is feeding it, so it only shows up once streaming starts.

**The tablet shows a black screen** — check `adb logcat -s extraspace`. A protocol
mismatch is reported explicitly on both sides.

## Roadmap

- [ ] Keyboard passthrough (`NotifyKeyboardKeycode` — the plumbing is already there)
- [ ] Stylus with pressure, for tablets with an active pen
- [ ] Wi-Fi transport, reusing the same protocol
- [ ] Audio to the tablet speakers
- [ ] Tablet microphone as a PipeWire source
- [ ] Camera controls: tap-to-focus, torch, zoom
- [ ] Follow tablet rotation live

## Development

```bash
cargo test                                          # unit tests, no hardware needed
cargo run -p xs-mutter --example virtual_monitor    # create a monitor for 5 seconds
RUST_LOG=debug cargo run                            # verbose
cd android && ./gradlew assembleRelease             # build the companion app
```

| Crate | Responsibility |
|---|---|
| `xs-proto` | Wire protocol, shared with the Kotlin side |
| `xs-mutter` | Virtual monitor + input injection over D-Bus |
| `xs-video` | PipeWire capture → H.264 |
| `xs-transport` | adb orchestration, framed sockets |
| `xs-camera` | H.264 → `/dev/video10` |
| `xs-core` | Session orchestration, adaptive bitrate |
| `xs-ui` | GTK4 / libadwaita front end |

`xs-mutter` is the only crate that touches mutter's private D-Bus API, so if a
future GNOME release changes it, the damage is contained to one file.

## Contributing

Issues and pull requests welcome. The most useful thing right now is testing on
other tablets and other GNOME versions — please include your GNOME version,
distribution, and tablet model.

## License

[GPL-3.0-or-later](LICENSE).

Note that the default encoder, x264, is itself GPL-licensed, so a distributed
binary would carry GPL obligations regardless. Licensing the project this way
keeps the situation unambiguous.
