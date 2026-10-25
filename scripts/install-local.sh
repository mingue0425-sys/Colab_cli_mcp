#!/bin/sh
# Build offline (toolchain/crates from /Volumes/game/Hycli/.cache) and refresh the
# local launcher in .local/bin. Data stays in .local/data.
set -e
cd "$(dirname "$0")/.."
. scripts/env.sh
cargo build --release --offline --locked
mkdir -p .local/bin
cp target/release/colab .local/bin/colab-bin.new
mv .local/bin/colab-bin.new .local/bin/colab-bin
.local/bin/colab --version
