# Contributing

## The most useful thing you can do

Run it on hardware that is not mine and say what happened. Extraspace is
verified on exactly one tablet and one GNOME version; everything beyond that is
untested. A [hardware report](https://github.com/Tymonoman/extraspace/issues/new?template=hardware_report.yml)
takes two minutes and is worth more than most patches right now — including the
boring "it just worked" ones.

## You do not need a tablet to work on this

Most of the project can be developed and tested without an Android device.

```bash
cargo test                                        # no hardware at all
cargo run -p xs-core --example fake_tablet        # whole host pipeline
cargo run -p xs-video --example capture_test      # capture and encode only
cargo run -p xs-mutter --example virtual_monitor  # just the mutter side
```

`fake_tablet` stands in for the device: it listens on the three sockets, does
the handshake, consumes the video stream and reports decode health. It exercises
session orchestration, virtual monitor creation, capture, encoding, framing over
real sockets, touch injection and the adaptive controller. The only things it
cannot cover are MediaCodec and USB itself.

You do need GNOME on Wayland, since the whole thing is built on mutter's D-Bus
APIs.

## Setting up

```bash
./scripts/setup.sh          # system packages, /dev/video10  (needs sudo)
./scripts/install.sh        # app grid entry                 (no sudo)
cd android && ./gradlew assembleRelease
```

## Before opening a PR

CI runs these, so running them first saves a round trip:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets   # CI treats warnings as errors
cargo test --workspace
```

## House style

**Comments explain why, not what.** The code already says what it does. What it
cannot say is that `vulkanh264enc` accepts a bitrate property and silently
ignores it, or that `adb forward` accepts TCP connections to nothing. Those
explanations are the reason this project was finishable, and several live in
comments precisely so the next person does not rediscover them the hard way.

**Keep mutter-specific code in `xs-mutter`.** It is a private, unstable D-Bus
API. Confining it to one crate means an upstream rename breaks one file rather
than the whole app.

**Prefer a test that would have caught the bug.** The adaptive controller is
pure and unit-tested for exactly this reason: it decides whether your display
goes blurry, and that should not require a tablet to verify.

## Where things live

| Crate | Responsibility |
|---|---|
| `xs-proto` | Wire protocol, mirrored by `Protocol.kt` on the Android side |
| `xs-mutter` | Virtual monitor and input injection over D-Bus |
| `xs-video` | PipeWire capture → H.264 |
| `xs-transport` | adb orchestration, framed sockets |
| `xs-camera` | H.264 → `/dev/video10` |
| `xs-core` | Session orchestration, adaptive bitrate |
| `xs-ui` | GTK4 / libadwaita front end |

If you change `xs-proto`, change `android/.../Protocol.kt` to match. They are
checked against each other by a magic number at runtime, so a mismatch fails
loudly on the first frame rather than corrupting quietly — but it still fails.

## Things that would help most

Roughly in order:

1. Testing on other tablets and GNOME versions.
2. Keyboard passthrough. Most of the groundwork exists in `xs-mutter::keys`
   and `InputOnlySession`; what is missing is wiring the Android side to it.
3. Wi-Fi transport. The protocol is transport-agnostic already; this is mostly
   discovery and a second `Transport` backend.
4. Stylus pressure, for tablets with an active pen.
