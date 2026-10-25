# Source this to build offline with the toolchain and crate cache kept in /Volumes/game/Hycli/.cache.
H="${HYCLI_CACHE:-/Volumes/game/Hycli/.cache}"
export PATH="$H/rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export RUSTUP_HOME="$H/rustup" CARGO_HOME="$H/cargo"
