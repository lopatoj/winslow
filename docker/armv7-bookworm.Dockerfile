# Cross-compilation image for the PocketBeagle running Debian 12 (bookworm).
#
# Building against the same Debian release as the board means the armhf
# libraries we link (libiio, libsystemd, libasound) and glibc match what's installed on
# the device. Used by `cross` via Cross.toml; cross mounts the host's Rust
# toolchain, so only the C toolchain, target libraries and QEMU live here.

FROM debian:bookworm-slim

ARG DEBIAN_FRONTEND=noninteractive
RUN dpkg --add-architecture armhf \
 && apt-get update \
 && apt-get install --assume-yes --no-install-recommends \
        ca-certificates \
        gcc libc6-dev pkg-config \
        gcc-arm-linux-gnueabihf libc6-dev-armhf-cross \
        qemu-user \
        libiio-dev:armhf \
        libsystemd-dev:armhf \
        libasound2-dev:armhf \
 && rm -rf /var/lib/apt/lists/*

ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUNNER=qemu-arm \
    CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
    AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR_armv7_unknown_linux_gnueabihf=/usr/lib/arm-linux-gnueabihf/pkgconfig:/usr/share/pkgconfig \
    # armhf shared libraries (ld-linux-armhf.so.3 etc.) live in the multiarch
    # paths, so qemu-arm needs no sysroot prefix to run test binaries.
    QEMU_LD_PREFIX=/
