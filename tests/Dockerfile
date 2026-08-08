ARG PLAYWRIGHT_VERSION=1.61.1
FROM ghcr.io/astral-sh/uv:latest AS uv
FROM mcr.microsoft.com/playwright:v${PLAYWRIGHT_VERSION}-noble

ARG RUST_VERSION=1.96.0
ARG TOXIPROXY_VERSION=2.12.0
ARG TARGETARCH

COPY --from=uv /uv /uvx /usr/local/bin/

RUN apt-get update \
    && apt-get install --yes --no-install-recommends build-essential ca-certificates cmake curl docker.io git gnupg jq lcov libssl-dev lsb-release pkg-config software-properties-common \
    && curl --fail --location --output /tmp/llvm.sh https://apt.llvm.org/llvm.sh \
    && chmod +x /tmp/llvm.sh \
    && /tmp/llvm.sh 22 \
    && rm -rf /var/lib/apt/lists/* /tmp/llvm.sh

ENV PATH="/root/.cargo/bin:/root/.local/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rustup component add clippy llvm-tools-preview rustfmt \
    && cargo install --locked cargo-leptos cargo-llvm-cov cargo-nextest just \
    && uv tool install pre-commit \
    && uv tool install twine==6.2.0 \
    && curl --fail --location --output /usr/local/bin/toxiproxy-server \
        "https://github.com/Shopify/toxiproxy/releases/download/v${TOXIPROXY_VERSION}/toxiproxy-server-linux-${TARGETARCH}" \
    && chmod +x /usr/local/bin/toxiproxy-server

WORKDIR /workspace
