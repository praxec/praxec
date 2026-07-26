#!/bin/sh
# Praxec gateway binary installer.
#
# Downloads the prebuilt `praxec` binary for your platform from the latest
# GitHub release, verifies its checksum, and installs it to ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/praxec/praxec/main/install.sh | sh
#
# Overrides (env):
#   PRAXEC_VERSION   tag to install (default: latest) — e.g. v0.0.30
#   PRAXEC_BIN_DIR   install directory (default: $HOME/.local/bin)
#
# Windows: download the .zip from the releases page instead.
set -eu

REPO="praxec/praxec"
VERSION="${PRAXEC_VERSION:-latest}"
BIN_DIR="${PRAXEC_BIN_DIR:-$HOME/.local/bin}"

say()  { printf '\033[1;36m▸\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64)              target="x86_64-unknown-linux-musl" ;;
  Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin:x86_64)             target="x86_64-apple-darwin" ;;
  Darwin:arm64)              target="aarch64-apple-darwin" ;;
  *) die "unsupported platform '$os/$arch' — download a bundle manually from https://github.com/$REPO/releases (Windows: the .zip)." ;;
esac

asset="praxec-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ($VERSION)"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "download failed: $base/$asset"

# Verify against the release's checksums.sha256. The asset is only installed
# if its hash matches; a missing checksums file is a loud warning, not a
# silent skip.
if curl -fsSL "$base/checksums.sha256" -o "$tmp/checksums.sha256" 2>/dev/null; then
  want="$(awk -v a="$asset" '$2 == a {print $1}' "$tmp/checksums.sha256")"
  [ -n "$want" ] || die "no checksum entry for $asset in checksums.sha256"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
  else
    got=""
    warn "no sha256 tool (sha256sum/shasum) found — cannot verify checksum"
  fi
  if [ -n "$got" ]; then
    [ "$got" = "$want" ] || die "checksum mismatch for $asset (want $want, got $got)"
    say "Checksum verified"
  fi
else
  warn "checksums.sha256 not published for this release — skipping verification"
fi

say "Extracting"
tar -xzf "$tmp/$asset" -C "$tmp" praxec || die "could not extract 'praxec' from $asset"

mkdir -p "$BIN_DIR"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$tmp/praxec" "$BIN_DIR/praxec" || die "failed to install to $BIN_DIR"
else
  cp "$tmp/praxec" "$BIN_DIR/praxec" && chmod 0755 "$BIN_DIR/praxec" || die "failed to install to $BIN_DIR"
fi

say "Installed praxec -> $BIN_DIR/praxec"
# Smoke-test the installed binary. A download that can't run (e.g. a
# glibc-too-new build on an older distro) is a failed install, not a success.
if ! "$BIN_DIR/praxec" --version; then
  die "praxec was downloaded to $BIN_DIR/praxec but does not run on this system \
(see the error above). If it names a missing GLIBC version, this build is too new \
for your distro — grab an older release or build from source."
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *)
    warn "$BIN_DIR is not on your PATH. Add it to your shell profile:"
    printf '    export PATH="%s:$PATH"\n' "$BIN_DIR" >&2
    ;;
esac
