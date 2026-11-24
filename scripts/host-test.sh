#!/bin/sh
# Run cargo natively on this machine inside Debian 12 with the board's system
# libraries, using your own Rust toolchain. Extra arguments go to cargo:
#
#   scripts/host-test.sh                 # all tests
#   scripts/host-test.sh -p hardware     # one crate
set -eu
cd "$(dirname "$0")/.."
# With no arguments, test the whole workspace.
[ $# -eq 0 ] && set -- --workspace
docker build --quiet --tag winslow-host-test --file docker/host-test.Dockerfile docker >/dev/null
exec docker run --rm --user "$(id -u):$(id -g)" \
    -v "$HOME/.cargo:$HOME/.cargo" -v "$HOME/.rustup:$HOME/.rustup" \
    -e CARGO_HOME="$HOME/.cargo" -e RUSTUP_HOME="$HOME/.rustup" -e HOME=/tmp \
    -v "$PWD:/w" -w /w winslow-host-test \
    "$HOME/.cargo/bin/cargo" test \
    --target x86_64-unknown-linux-gnu --target-dir target/host-test "$@"
