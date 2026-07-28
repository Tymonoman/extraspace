#!/usr/bin/env bash
# Installs Extraspace into your user account: binary, icon and desktop entry.
#
# Everything lands under ~/.local, so this needs no root and touches nothing
# system-wide. `./scripts/setup.sh` is the one that needs sudo, and it handles a
# different job -- system packages and the virtual camera device.
#
#   ./scripts/install.sh              # build if needed, then install
#   ./scripts/install.sh --uninstall  # remove everything this installed
set -euo pipefail

APP_ID="io.github.tymonoman.Extraspace"
BIN_NAME="extraspace"

REPO_ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
DESKTOP_DIR="$DATA_DIR/applications"
ICON_DIR="$DATA_DIR/icons/hicolor/scalable/apps"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }

if [[ "${1:-}" == "--uninstall" ]]; then
  bold "Uninstalling Extraspace"
  rm -f "$BIN_DIR/$BIN_NAME" && ok "removed $BIN_DIR/$BIN_NAME"
  rm -f "$DESKTOP_DIR/$APP_ID.desktop" && ok "removed desktop entry"
  rm -f "$ICON_DIR/$APP_ID.svg" && ok "removed icon"
  update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
  gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" 2>/dev/null || true
  echo
  echo "Your settings in ~/.config/extraspace were left alone."
  exit 0
fi

bold "Installing Extraspace"
echo

# --- build -------------------------------------------------------------------
BINARY="$REPO_ROOT/target/release/$BIN_NAME"
if [[ ! -x "$BINARY" ]]; then
  warn "no release build found, building now (this takes a few minutes)"
  ( cd "$REPO_ROOT" && cargo build --release )
fi
[[ -x "$BINARY" ]] || { echo "build produced no binary at $BINARY"; exit 1; }

# --- binary ------------------------------------------------------------------
# Copied rather than symlinked: a symlink into target/ breaks the moment anyone
# runs `cargo clean`, and the failure ("No such file") gives no hint why.
mkdir -p "$BIN_DIR"
install -m755 "$BINARY" "$BIN_DIR/$BIN_NAME"
ok "installed $BIN_DIR/$BIN_NAME"

# --- icon --------------------------------------------------------------------
mkdir -p "$ICON_DIR"
install -m644 "$REPO_ROOT/packaging/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg"
ok "installed icon"

# --- desktop entry -----------------------------------------------------------
mkdir -p "$DESKTOP_DIR"
# Written out rather than copied so Exec points at the absolute installed path.
# GNOME will happily launch a bare `extraspace` only if ~/.local/bin is on PATH,
# and for a GUI launch that PATH comes from the session, not your shell rc --
# so a relative Exec silently fails to start for some users.
cat > "$DESKTOP_DIR/$APP_ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Extraspace
GenericName=Tablet Display
Comment=Use an Android tablet as an extra display and webcam
Exec=$BIN_DIR/$BIN_NAME
Icon=$APP_ID
Terminal=false
Categories=Utility;GTK;GNOME;
Keywords=display;monitor;tablet;android;screen;webcam;camera;second screen;
StartupNotify=true
StartupWMClass=$BIN_NAME
EOF
ok "installed desktop entry"

update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" 2>/dev/null || true

# --- PATH check --------------------------------------------------------------
echo
case ":$PATH:" in
  *":$BIN_DIR:"*) ok "$BIN_DIR is on your PATH" ;;
  *) warn "$BIN_DIR is not on your PATH, so \`$BIN_NAME\` will not work in a terminal."
     warn "The app grid launcher works regardless. To fix the terminal case:"
     echo "      echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
esac

echo
bold "Done."
echo "Find \"Extraspace\" in your applications, or run: $BIN_NAME"
echo "Remove it again with: ./scripts/install.sh --uninstall"
