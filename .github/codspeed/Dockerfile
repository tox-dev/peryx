ARG UBUNTU_IMAGE=ubuntu:24.04@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90
FROM ${UBUNTU_IMAGE} AS artifacts

ADD --checksum=sha256:e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c \
    https://static.rust-lang.org/rustup/archive/1.28.2/aarch64-unknown-linux-gnu/rustup-init /artifacts/rustup-init
ADD --checksum=sha256:0e4d7f4c9cce5130c9cf9b8ce69ae988fcf8c14c2562dcb7e4c0e4e81fac71f8 \
    https://github.com/CodSpeedHQ/codspeed-rust/releases/download/v5.0.1/cargo-codspeed-aarch64-unknown-linux-musl \
    /artifacts/cargo-codspeed
ADD --checksum=sha256:115957037b94626e8788ced04664f2421d093fb4e8504c23c93d95e3108837e3 \
    https://github.com/CodSpeedHQ/codspeed/releases/download/v4.18.4/codspeed-runner-aarch64-unknown-linux-musl.tar.gz \
    /artifacts/codspeed.tar.gz
ADD --checksum=sha256:47e71ef8a095a8dc26e31e7a5d66c3665ff2d68a3f0350d351d7affe3aec2944 \
    https://github.com/CodSpeedHQ/valgrind-codspeed/releases/download/3.26.0-0codspeed6/valgrind_3.26.0-0codspeed6_ubuntu-24.04_arm64.deb \
    /artifacts/valgrind.deb

FROM ${UBUNTU_IMAGE}

LABEL org.opencontainers.image.source="https://github.com/tox-dev/peryx"
LABEL org.opencontainers.image.description="Reproducible CodSpeed runtime for peryx"
LABEL org.opencontainers.image.licenses="MIT"

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    RUSTUP_TOOLCHAIN=1.96.0-aarch64-unknown-linux-gnu \
    PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    DEBIAN_FRONTEND=noninteractive

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN --mount=from=artifacts,source=/artifacts,target=/artifacts \
    apt-get update \
    && apt-get install --yes --no-install-recommends \
        /artifacts/valgrind.deb \
        build-essential \
        ca-certificates \
        git \
        libc6-dbg \
        pkg-config \
        util-linux \
        zstd \
    && rm -rf /var/lib/apt/lists/*

RUN --mount=from=artifacts,source=/artifacts,target=/artifacts \
    install -m 0755 /artifacts/rustup-init /tmp/rustup-init \
    && /tmp/rustup-init --default-toolchain 1.96.0 --no-modify-path --profile minimal -y \
    && rustup component add clippy llvm-tools-preview rustfmt \
    && install -m 0755 /artifacts/cargo-codspeed /usr/local/bin/cargo-codspeed \
    && mkdir /tmp/codspeed \
    && tar -xzf /artifacts/codspeed.tar.gz --strip-components=1 -C /tmp/codspeed \
    && install -m 0755 /tmp/codspeed/codspeed /usr/local/bin/codspeed \
    && test -x /usr/local/bin/cargo-codspeed \
    && test -x /usr/local/bin/codspeed \
    && valgrind --version \
    && rm -rf /tmp/codspeed /tmp/rustup-init
