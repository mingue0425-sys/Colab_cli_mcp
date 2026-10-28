#!/bin/sh
# Install the Rust toolchain and fetch crates into .local, so builds need
# nothing outside this folder. Safe to re-run.
set -e
cd "$(dirname "$0")/.."
L="$PWD/.local"
export RUSTUP_HOME="$L/rustup" CARGO_HOME="$L/cargo-home"
if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
    case "$(uname -m)" in arm64 | aarch64) arch=aarch64 ;; *) arch=x86_64 ;; esac
    case "$(uname -s)" in Darwin) os=apple-darwin ;; *) os=unknown-linux-gnu ;; esac
    url="https://static.rust-lang.org/rustup/dist/$arch-$os/rustup-init"
    mkdir -p "$L/download"
    curl -sSf "$url" -o "$L/download/rustup-init"
    expected=$(curl -sSf "$url.sha256" | cut -d' ' -f1)
    actual=$(shasum -a 256 "$L/download/rustup-init" | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || { echo "rustup-init checksum mismatch" >&2; exit 1; }
    chmod +x "$L/download/rustup-init"
    "$L/download/rustup-init" -y --no-modify-path --profile minimal \
        --default-toolchain stable -c clippy -c rustfmt
fi
"$CARGO_HOME/bin/cargo" fetch --locked
"$CARGO_HOME/bin/cargo" --version
