#!/usr/bin/env bash
# One-time host setup for extraspace.
#
# Everything here needs root, which is why it is a separate script the user runs
# once rather than something the app tries to do at runtime. After this, the app
# itself runs completely unprivileged.
#
#   ./scripts/setup.sh            # install + configure
#   ./scripts/setup.sh --check    # report status, change nothing
set -euo pipefail

CHECK_ONLY=0
[[ "${1:-}" == "--check" ]] && CHECK_ONLY=1

VIDEO_NR=10
CARD_LABEL="Extraspace Tablet Camera"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }

# Build-time headers, plus the runtime pieces the pipeline needs.
BUILD_DEPS=(
  gcc pkgconf-pkg-config
  gstreamer1-devel gstreamer1-plugins-base-devel
  gtk4-devel libadwaita-devel
)
RUNTIME_DEPS=(
  gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free
  pipewire-gstreamer
  android-tools
  v4l2loopback
)
# x264enc: much faster and better quality than the openh264 fallback. Lives in
# RPM Fusion, so it is optional rather than required -- the app works without it.
OPTIONAL_DEPS=(gstreamer1-plugins-ugly)

missing_packages() {
  local pkgs=("$@") missing=()
  for p in "${pkgs[@]}"; do
    rpm -q "$p" &>/dev/null || missing+=("$p")
  done
  printf '%s\n' "${missing[@]:-}"
}

bold "extraspace setup"
echo

# ---------------------------------------------------------------- environment
bold "1. Checking environment"
if [[ "${XDG_SESSION_TYPE:-}" == "wayland" ]]; then
  ok "Wayland session"
else
  warn "session type is '${XDG_SESSION_TYPE:-unknown}', expected wayland"
fi
if [[ "${XDG_CURRENT_DESKTOP:-}" == *GNOME* ]]; then
  ok "GNOME desktop"
else
  bad "desktop is '${XDG_CURRENT_DESKTOP:-unknown}'. extraspace needs GNOME:"
  bad "it drives mutter's virtual-monitor API, which other compositors do not have."
fi
if command -v gnome-shell &>/dev/null; then
  ok "$(gnome-shell --version)"
fi
echo

# ---------------------------------------------------------------- packages
bold "2. Packages"
mapfile -t need_build   < <(missing_packages "${BUILD_DEPS[@]}")
mapfile -t need_runtime < <(missing_packages "${RUNTIME_DEPS[@]}")
mapfile -t need_opt     < <(missing_packages "${OPTIONAL_DEPS[@]}")
need_build=("${need_build[@]:-}");   need_build=("${need_build[@]//}")
to_install=()
for p in "${need_build[@]}" "${need_runtime[@]}"; do [[ -n "$p" ]] && to_install+=("$p"); done

if ((${#to_install[@]} == 0)); then
  ok "all required packages present"
else
  warn "missing: ${to_install[*]}"
  if ((CHECK_ONLY == 0)); then
    sudo dnf install -y "${to_install[@]}"
    ok "installed"
  fi
fi

for p in "${need_opt[@]}"; do
  [[ -z "$p" ]] && continue
  warn "$p not installed (optional: gives x264enc, ~2.5x faster than openh264)"
  if ((CHECK_ONLY == 0)); then
    sudo dnf install -y "$p" || warn "could not install $p -- is RPM Fusion enabled? Continuing."
  fi
done
echo

# ---------------------------------------------------------------- v4l2loopback
bold "3. Virtual camera device (/dev/video$VIDEO_NR)"
# exclusive_caps=1 makes the device advertise itself as a capture device only
# while something is actually feeding it, so it does not clutter the camera list
# in Chrome, Zoom and friends when extraspace is not running.
if [[ -e "/dev/video$VIDEO_NR" ]]; then
  ok "/dev/video$VIDEO_NR exists"
elif ((CHECK_ONLY == 1)); then
  warn "/dev/video$VIDEO_NR does not exist yet"
else
  sudo tee /etc/modprobe.d/extraspace.conf >/dev/null <<EOF
# Virtual webcam fed by the extraspace Android tablet camera.
options v4l2loopback video_nr=$VIDEO_NR card_label="$CARD_LABEL" exclusive_caps=1 max_buffers=2
EOF
  echo v4l2loopback | sudo tee /etc/modules-load.d/extraspace.conf >/dev/null
  sudo modprobe -r v4l2loopback 2>/dev/null || true
  sudo modprobe v4l2loopback
  if [[ -e "/dev/video$VIDEO_NR" ]]; then
    ok "created /dev/video$VIDEO_NR (\"$CARD_LABEL\"), persists across reboots"
  else
    bad "v4l2loopback loaded but /dev/video$VIDEO_NR did not appear"
  fi
fi
echo

# ---------------------------------------------------------------- adb
bold "4. Tablet"
if ! command -v adb &>/dev/null; then
  bad "adb not found (install android-tools)"
else
  state=$(adb devices | awk 'NR>1 && NF {print $2; exit}')
  case "${state:-none}" in
    device)
      ok "tablet connected and authorised"
      ;;
    unauthorized)
      bad "tablet is connected but NOT authorised."
      bad "Unlock it and accept the 'Allow USB debugging?' prompt,"
      bad "ticking 'Always allow from this computer'."
      ;;
    none)
      warn "no tablet detected. Connect it by USB and enable:"
      warn "  Settings -> About tablet -> tap 'Build number' 7 times"
      warn "  Settings -> System -> Developer options -> USB debugging"
      ;;
    *)
      warn "tablet in state '$state'"
      ;;
  esac
fi
echo

# ---------------------------------------------------------------- encoders
bold "5. Encoders"
for e in x264enc openh264enc; do
  if gst-inspect-1.0 "$e" &>/dev/null; then ok "$e available"; else warn "$e not available"; fi
done
if ! gst-inspect-1.0 x264enc &>/dev/null && ! gst-inspect-1.0 openh264enc &>/dev/null; then
  bad "no usable H.264 encoder -- extraspace cannot stream without one"
fi
echo

bold "Done."
((CHECK_ONLY == 1)) && echo "(--check: nothing was changed)"
echo "Next:  cargo run --release"
