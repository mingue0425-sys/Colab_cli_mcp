# Source this to build with the toolchain and crate cache kept in this repo's
# .local (set up once by scripts/bootstrap.sh); nothing is installed elsewhere.
L="$(cd "$(dirname "${BASH_SOURCE[0]:-${(%):-%x}}")/.." && pwd)/.local"
export RUSTUP_HOME="$L/rustup" CARGO_HOME="$L/cargo-home"
export PATH="$CARGO_HOME/bin:$PATH"
