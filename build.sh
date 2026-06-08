#!/usr/bin/env bash
# picode build tool. Usage:
#   ./build.sh           cross-compile for the Pi Zero W (ARMv6, static musl)
#   ./build.sh deploy    build + install on the Pi + sync source to ~/picode
#   ./build.sh pull      copy the Pi's ~/picode source back to the Mac (recover
#                        on-device self-edits before rebuilding)
set -eo pipefail

TARGET=arm-unknown-linux-musleabihf
PI="${PI:-bluevisor@10.0.0.216}"
CROSS=arm-unknown-linux-musleabihf
CMD="${1:-build}"

cd "$(dirname "$0")"

# ---------------------------------------------------------------- pull -------
if [[ "$CMD" == "pull" ]]; then
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  echo ">> pulling $PI:~/picode -> $TMP ..."
  ssh "$PI" 'cd ~/picode && tar czf - --exclude target --exclude ".git" .' \
    | tar xzf - -C "$TMP"

  # Program source we own; build.sh is handled separately (see below) to avoid
  # overwriting this script while it runs.
  FILES=(src Cargo.toml Cargo.lock .cargo PICODE.md)

  echo ">> changes (Pi vs Mac):"
  changed=0
  for f in "${FILES[@]}"; do
    if [ -e "$TMP/$f" ]; then
      if ! diff -ruN "$f" "$TMP/$f" >/dev/null 2>&1; then
        diff -ruN "$f" "$TMP/$f" || true
        changed=1
      fi
    fi
  done
  # build.sh: report only, never auto-overwrite the running script.
  if [ -e "$TMP/build.sh" ] && ! diff -q build.sh "$TMP/build.sh" >/dev/null 2>&1; then
    cp "$TMP/build.sh" build.sh.pi
    echo "   NOTE: Pi build.sh differs — saved as build.sh.pi for manual review"
    changed=1
  fi

  if [ "$changed" -eq 0 ]; then
    echo "   no differences — Mac source already matches the Pi."
    exit 0
  fi

  BAK="/tmp/picode-mac-src-backup-$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$BAK"
  cp -R src Cargo.toml Cargo.lock .cargo PICODE.md "$BAK"/ 2>/dev/null || true
  echo ">> backed up current Mac source to $BAK"

  for f in "${FILES[@]}"; do
    [ -e "$TMP/$f" ] && cp -R "$TMP/$f" "./"
  done
  echo ">> pulled. Review the diff above, then: ./build.sh deploy"
  exit 0
fi

# ----------------------------------------------------------- build/deploy ----
export CC_arm_unknown_linux_musleabihf=${CROSS}-gcc
export AR_arm_unknown_linux_musleabihf=${CROSS}-ar
export TARGET_CC=${CROSS}-gcc

echo ">> building $TARGET (release)..."
cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/picode"
ls -lh "$BIN"
file "$BIN"

if [[ "$CMD" == "deploy" ]]; then
  echo ">> deploying to $PI..."
  scp -q "$BIN" "$PI:/tmp/picode.new"
  ssh "$PI" '
    set -e
    mkdir -p ~/.local/bin
    if [ -f ~/.local/bin/picode ] && ! [ -f ~/.local/bin/picode-py ]; then
      cp ~/.local/bin/picode ~/.local/bin/picode-py
      echo "   backed up Python picode -> picode-py"
    fi
    mv /tmp/picode.new ~/.local/bin/picode
    chmod +x ~/.local/bin/picode
    echo "   installed: $(~/.local/bin/picode --version)"
  '
  # Keep a self-editable source copy on the Pi in sync (~/picode).
  echo ">> syncing source to $PI:~/picode..."
  COPYFILE_DISABLE=1 tar czf - --exclude target --exclude .git \
    Cargo.toml Cargo.lock .cargo build.sh PICODE.md src \
    | ssh "$PI" 'mkdir -p ~/picode && tar xzf - -C ~/picode && find ~/picode -name "._*" -delete'
  echo "   synced source"
fi
