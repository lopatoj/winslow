# Debian 12 (like the board) for the laptop's own architecture, with the
# system libraries winslowd links against. Used by scripts/host-test.sh to
# run the tests natively, e.g. ones that need a real pseudo-terminal, which
# the qemu-based `cross test` can't provide.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install --assume-yes --no-install-recommends \
        ca-certificates gcc libc6-dev pkg-config \
        libiio-dev libsystemd-dev libasound2-dev \
 && rm -rf /var/lib/apt/lists/*
