#!/bin/sh
# jfd のインストーラ / installer for jfd
#   curl -fsSL https://raw.githubusercontent.com/masa-whitestone/jev-find/main/install.sh | sh
# 入れる場所 / install dir: $JFD_INSTALL_DIR (default: ~/.local/bin)
set -eu

REPO="masa-whitestone/jev-find"
DIR="${JFD_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  *)
    echo "jfd: no prebuilt binary for $(uname -s) $(uname -m)." >&2
    echo "     install from source: cargo install --git https://github.com/$REPO" >&2
    exit 1
    ;;
esac

base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading jfd ($target)..."
curl -fsSL "$base/jfd-$target" -o "$tmp/jfd"
curl -fsSL "$base/jfd-$target.sha256" -o "$tmp/jfd.sha256"

want="$(cut -d' ' -f1 "$tmp/jfd.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp/jfd" | cut -d' ' -f1)"
else
  got="$(shasum -a 256 "$tmp/jfd" | cut -d' ' -f1)"
fi
if [ "$want" != "$got" ]; then
  echo "jfd: checksum mismatch; aborting." >&2
  exit 1
fi

mkdir -p "$DIR"
chmod 755 "$tmp/jfd"
mv "$tmp/jfd" "$DIR/jfd"
echo "Installed: $DIR/jfd ($("$DIR/jfd" --version))"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "Add it to your PATH:  export PATH=\"$DIR:\$PATH\"" ;;
esac
echo "Next: jfd setup"
