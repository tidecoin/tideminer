#!/bin/sh
# tideminer installer for macOS and Linux:
#
#   curl -fsSL https://github.com/tidecoin/tideminer/releases/latest/download/install.sh | sh
#
# Downloads the release archive for this OS and CPU, checks it against the release's
# SHA256SUMS, and installs `tideminer` into ~/.local/bin (no sudo). Re-run to update.
#
# Environment:
#   TIDEMINER_VERSION      release tag to install, e.g. v0.2.0 (default: latest)
#   TIDEMINER_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   TIDEMINER_REPO         GitHub owner/repo that publishes releases
#   TIDEMINER_BASE_URL     download from this URL instead of GitHub Releases
set -eu

repo="${TIDEMINER_REPO:-tidecoin/tideminer}"
if [ -n "${TIDEMINER_BASE_URL:-}" ]; then
  base="$TIDEMINER_BASE_URL"
elif [ -n "${TIDEMINER_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$TIDEMINER_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi
dir="${TIDEMINER_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'tideminer install: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Darwin/*) asset=tideminer-macos-universal.tar.gz ;;
  Linux/x86_64 | Linux/amd64) asset=tideminer-linux-x86_64.tar.gz ;;
  Linux/aarch64 | Linux/arm64) asset=tideminer-linux-arm64.tar.gz ;;
  *) fail "no build for $os $arch (Windows: use install.ps1)" ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --proto '=https,http' --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "needs curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "needs sha256sum or shasum to verify the download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset"
fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "$asset is not listed in SHA256SUMS"
actual=$(sha256 "$tmp/$asset")
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"
say "checksum ok"

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/tideminer" ] || fail "archive does not contain tideminer"
chmod 755 "$tmp/tideminer"
if [ "$os" = Darwin ]; then
  # curl does not quarantine files, but other download paths might have.
  xattr -d com.apple.quarantine "$tmp/tideminer" 2>/dev/null || true
fi

say "checking downloaded binary"
"$tmp/tideminer" self-test >/dev/null || fail "downloaded binary failed its self-test; existing installation was not changed"
version=$("$tmp/tideminer" --version) || fail "could not read downloaded binary version; existing installation was not changed"

mkdir -p "$dir"
# Copy next to the target, then rename: a running miner keeps its old file.
cp "$tmp/tideminer" "$dir/.tideminer.new"
mv -f "$dir/.tideminer.new" "$dir/tideminer"

say "installed $version to $dir/tideminer (self-test passed)"
say "If tideminer is running, restart it with your usual settings to use this version."

case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    say ""
    say "$dir is not in your PATH. Add it, e.g.:"
    case "${SHELL:-}" in
      */zsh) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
      */fish) say "  fish_add_path $dir" ;;
      *) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.bashrc && exec bash" ;;
    esac
    ;;
esac

say ""
say "start mining:"
say "  tideminer -o POOL_HOST:PORT --tls -u YOUR_TDC_ADDRESS.rig"
say "tune for this machine first (optional, a few minutes):"
if [ "$os" = Darwin ]; then
  say "  tideminer tune          # add --gpu to include the GPU"
else
  say "  tideminer tune"
fi
