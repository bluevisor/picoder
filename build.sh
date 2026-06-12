#!/usr/bin/env bash
# picode build tool. Usage:
#   ./build.sh           cross-compile for the Pi Zero W (ARMv6, static musl)
#   ./build.sh deploy    build + install + sync source to every host in PICODE_HOSTS
#   ./build.sh pull      copy a Pi's ~/picode source back to the Mac (recover
#                        on-device self-edits before rebuilding)
#
# Deploy targets are configurable — no IPs are hardcoded in the logic:
#   PICODE_HOSTS   space-separated list of user@host (default below)
#   PI=user@host   target a single host (overrides PICODE_HOSTS, used by pull too)
# Examples:
#   PICODE_HOSTS="pi@10.1.2.3 pi@10.1.2.4" ./build.sh deploy
#   PI=bluevisor@10.0.0.128 ./build.sh deploy
# You can also export PICODE_HOSTS in your shell profile to set it permanently.
set -eo pipefail

CMD="${1:-build}"

# Default deploy targets. Each host gets the binary matching its architecture
# (Pi Zero W = ARMv6, Pi 5 = aarch64). Override with PICODE_HOSTS, or PI=... for
# a single host.
DEFAULT_HOSTS="bluevisor@10.0.0.216 bluevisor@10.0.0.128"
read -r -a PIS <<<"${PICODE_HOSTS:-$DEFAULT_HOSTS}"
if [[ -n "${PI:-}" ]]; then
  PIS=("$PI")
fi
if [[ ${#PIS[@]} -eq 0 ]]; then
  echo "!! no deploy hosts — set PICODE_HOSTS or PI" >&2
  exit 1
fi
# pull is inherently single-host: use PI if given, else the last configured host.
PULL_PI="${PI:-${PIS[${#PIS[@]}-1]}}"

cd "$(dirname "$0")"

# ---------------------------------------------------------------- pull -------
if [[ "$CMD" == "pull" ]]; then
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  echo ">> pulling $PULL_PI:~/picode -> $TMP ..."
  ssh "$PULL_PI" 'cd ~/picode && tar czf - --exclude target --exclude ".git" .' \
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
# Three targets, all static musl: ARMv6 for the Pi Zero W, ARMv7 for 32-bit
# Pi OS on the Pi 2/3/4 (armv7l), and aarch64 for the Pi 5 / 64-bit boxes
# (running the 32-bit ARMv6 binary under compat is fragile and can segfault,
# so 64-bit hosts get a native build).
TARGET_ARMV6=arm-unknown-linux-musleabihf
TARGET_ARMV7=armv7-unknown-linux-musleabihf
TARGET_ARM64=aarch64-unknown-linux-musl

# Map a remote `uname -m` to the Rust target triple we deploy there.
target_for_arch() {
  case "$1" in
    aarch64|arm64)  echo "$TARGET_ARM64" ;;
    armv7l)         echo "$TARGET_ARMV7" ;;
    armv6l|arm)     echo "$TARGET_ARMV6" ;;
    *)              echo "" ;;
  esac
}

# Build one target (idempotent — cargo no-ops if already up to date). The
# ARMv7 build reuses the ARMv6 musl gcc as linker/CC (see .cargo/config.toml).
build_target() {
  local t="$1"
  echo ">> building $t (release)..."
  case "$t" in
    "$TARGET_ARMV6")
      CC_arm_unknown_linux_musleabihf=${TARGET_ARMV6}-gcc \
      AR_arm_unknown_linux_musleabihf=${TARGET_ARMV6}-ar \
      TARGET_CC=${TARGET_ARMV6}-gcc \
        cargo build --release --target "$t" ;;
    "$TARGET_ARMV7")
      CC_armv7_unknown_linux_musleabihf=${TARGET_ARMV6}-gcc \
      AR_armv7_unknown_linux_musleabihf=${TARGET_ARMV6}-ar \
      TARGET_CC=${TARGET_ARMV6}-gcc \
        cargo build --release --target "$t" ;;
    "$TARGET_ARM64")
      CC_aarch64_unknown_linux_musl=${TARGET_ARM64}-gcc \
      AR_aarch64_unknown_linux_musl=${TARGET_ARM64}-ar \
      TARGET_CC=${TARGET_ARM64}-gcc \
        cargo build --release --target "$t" ;;
    *) echo "!! unknown target $t" >&2; return 1 ;;
  esac
}

if [[ "$CMD" != "deploy" ]]; then
  # Plain build: produce all three binaries.
  build_target "$TARGET_ARMV6"
  build_target "$TARGET_ARMV7"
  build_target "$TARGET_ARM64"
  for t in "$TARGET_ARMV6" "$TARGET_ARMV7" "$TARGET_ARM64"; do
    file "target/$t/release/picode"
  done
  exit 0
fi

# deploy: pick the right binary per host by querying its architecture.
for P in "${PIS[@]}"; do
  arch="$(ssh "$P" 'uname -m' | tr -d '\r')"
  t="$(target_for_arch "$arch")"
  if [[ -z "$t" ]]; then
    echo "!! $P: unsupported arch '$arch' — skipping" >&2
    continue
  fi
  build_target "$t"
  BIN="target/$t/release/picode"
  echo ">> deploying to $P ($arch -> $t)..."
  scp -q "$BIN" "$P:/tmp/picode.new"
  ssh "$P" '
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
  echo ">> syncing source to $P:~/picode..."
  COPYFILE_DISABLE=1 tar czf - --exclude target --exclude .git \
    Cargo.toml Cargo.lock .cargo build.sh PICODE.md src \
    | ssh "$P" 'mkdir -p ~/picode && tar xzf - -C ~/picode && find ~/picode -name "._*" -delete'
  echo "   synced source"
done
