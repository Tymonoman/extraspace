#!/usr/bin/env bash
# Records the demo GIF used in the README.
#
# Drives a real session end to end: starts Extraspace, stages a neutral scene on
# the virtual monitor, records the tablet's own screen, and converts the result
# to a GIF small enough for a README.
#
# The scene is deliberately synthetic. Recording whatever happens to be on your
# desktop publishes your files, your notifications and your window titles, so
# this opens a purpose-made document instead.
#
#   ./scripts/record-demo.sh [seconds]
set -euo pipefail

DURATION="${1:-12}"
REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
OUT_DIR="$REPO_ROOT/assets"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }

command -v adb >/dev/null || { echo "adb not found"; exit 1; }
[[ "$(adb devices | awk 'NR>1 && NF {print $2; exit}')" == "device" ]] \
  || { echo "no authorised tablet; see README"; exit 1; }

bold "Recording Extraspace demo (${DURATION}s)"

# --- the scene ---------------------------------------------------------------
cat > "$WORK/demo.txt" <<'SCENE'
                          Extraspace

    This window is running on an Android tablet.

    It is not a screenshot and not a mirror of another
    screen -- GNOME believes this is a real monitor.
    It has its own workspaces, it can be arranged in
    Settings -> Displays, and windows can be dragged
    onto it like any other output.

    Everything travels over the USB cable:

        desktop  ->  virtual monitor  ->  H.264
                 ->  USB  ->  MediaCodec  ->  here

    Touch goes back the other way, so tapping this
    screen moves the cursor on that monitor.

        first frame       47 ms
        round trip        ~1 ms
        the cable         USB 2.0, and not the bottleneck
SCENE

# --- run ---------------------------------------------------------------------
adb shell am force-stop io.github.tymonoman.extraspace 2>/dev/null || true
sleep 1

"$REPO_ROOT/target/release/extraspace" >"$WORK/host.log" 2>&1 &
HOST=$!
trap 'kill $HOST 2>/dev/null || true; rm -rf "$WORK"' EXIT

for _ in $(seq 20); do
  grep -q "tablet said hello" "$WORK/host.log" && break
  sleep 1
done
grep -q "tablet said hello" "$WORK/host.log" || { echo "session never came up"; exit 1; }
ok "session up"

# A terminal rather than a text editor: editors restore your previous tabs, so
# the recording would publish the names of whatever files you had open. A fresh
# terminal starts empty every time.
#
# btop gives the recording constant motion, which a GIF needs, and is also the
# honest hard case for the encoder: full-colour text changing every frame is
# what the bitrate actually has to cope with.
#
# Its process list is suppressed below. It would otherwise publish every running
# program, its command line and its home paths straight into the README.
mkdir -p "$WORK/btop/btop"
cat > "$WORK/btop/btop/btop.conf" <<'BTOP_CONF'
shown_boxes = "cpu mem net"
update_ms = 500
proc_tree = False
theme_background = True
truecolor = True
BTOP_CONF

cat > "$WORK/scene.sh" <<SCENE_SH
cat '$WORK/demo.txt'
printf '\n'
export XDG_CONFIG_HOME='$WORK/btop'
exec btop --force-utf 2>/dev/null || exec top
SCENE_SH
chmod +x "$WORK/scene.sh"

kitty --title "Extraspace" \
      -o font_size=14 -o background_opacity=1 -o cursor_blink_interval=0 \
      -o background="#1d1d20" -o foreground="#f6f5f4" \
      "$WORK/scene.sh" >/dev/null 2>&1 &
sleep 4

# Push the window onto the tablet, then fill the screen with it. Which monitor
# it opens on varies with your layout, so cycle rather than assume.
for _ in 1 2 3; do
  "$REPO_ROOT/target/release/examples/send_keys" "super+shift+right" >/dev/null 2>&1 || true
  sleep 1
done
"$REPO_ROOT/target/release/examples/send_keys" "super+up" >/dev/null 2>&1 || true
sleep 2
ok "scene staged"

adb shell screenrecord --time-limit "$DURATION" --bit-rate 8000000 /sdcard/xs-demo.mp4 2>/dev/null &
REC=$!
sleep 2
# Trace a path with a finger, so the recording shows touch actually driving the
# cursor rather than just a static picture.
for _ in 1 2 3; do
  adb shell input swipe 500 400 1500 850 1200 2>/dev/null || true
  adb shell input swipe 1500 850 600 500 1000 2>/dev/null || true
done
wait $REC 2>/dev/null || true
sleep 2
adb pull /sdcard/xs-demo.mp4 "$WORK/demo.mp4" >/dev/null 2>&1
adb shell rm /sdcard/xs-demo.mp4 2>/dev/null || true
ok "captured $(du -h "$WORK/demo.mp4" | cut -f1)"

pkill -f "kitty --title Extraspace" 2>/dev/null || true
kill $HOST 2>/dev/null || true

# --- encode ------------------------------------------------------------------
mkdir -p "$OUT_DIR"
# Two-pass palette: a naive GIF of a photographic desktop dithers into mud and
# comes out several times larger.
# 10fps at 760px keeps a README hero under about half a megabyte. Full-colour
# terminal output is close to the worst case for GIF, so this is tuned tighter
# than a screencast of ordinary UI would need.
ffmpeg -v error -y -i "$WORK/demo.mp4" \
  -vf "fps=10,scale=760:-1:flags=lanczos,palettegen=stats_mode=diff" "$WORK/pal.png"
ffmpeg -v error -y -i "$WORK/demo.mp4" -i "$WORK/pal.png" \
  -lavfi "fps=10,scale=760:-1:flags=lanczos[v];[v][1:v]paletteuse=dither=bayer:bayer_scale=4" \
  "$OUT_DIR/demo.gif"

ffmpeg -v error -y -i "$WORK/demo.mp4" \
  -vf "fps=24,scale=1100:-1:flags=lanczos" -c:v libx264 -preset slow -crf 26 \
  -pix_fmt yuv420p -movflags +faststart "$OUT_DIR/demo.mp4"

ok "assets/demo.gif  $(du -h "$OUT_DIR/demo.gif" | cut -f1)"
ok "assets/demo.mp4  $(du -h "$OUT_DIR/demo.mp4" | cut -f1)"
echo
bold "Done."
