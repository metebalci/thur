# Thur release builder image (thurvtl + thurvsa).
#
# Built on Debian 11 (Bullseye) to pin the glibc floor at 2.31. Binaries
# produced here run on every distro Thur targets commercially:
# RHEL 9 (glibc 2.34), RHEL 10, SLES 15 SP3+ (glibc 2.31+), SLES 16,
# Debian 12/13, Ubuntu 24.04/26.04.
#
# OpenSSL is forced to vendored mode in shared/cloud and shared/keystore
# (their Cargo.toml carry the `features = ["vendored"]` pin), so this
# image doesn't need libssl-dev — perl + a C toolchain are enough for
# the openssl-src build.
FROM debian:11

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_TERM_COLOR=always
ENV RUSTUP_HOME=/opt/rust
ENV CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:$PATH

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        build-essential \
        pkg-config \
        perl \
        make \
        cmake \
        gnupg \
        git \
    && rm -rf /var/lib/apt/lists/*

# Pin to a specific stable Rust. Floor is set by whichever transitive
# dep has the highest `rust-version` requirement — currently aws-sdk-*
# at 1.91.1. To find the current floor:
#   cargo build 2>&1 | grep "requires rustc"
# and bump this ARG to match. Bump deliberately; the cargo-install of
# cargo-deb / cargo-generate-rpm at the next layer is the canary.
ARG RUST_VERSION=1.92.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain ${RUST_VERSION} --profile minimal --no-modify-path \
    && rustc --version && cargo --version

# Pure-Rust packagers — neither needs dpkg/rpm tooling.
RUN cargo install --locked cargo-deb \
    && cargo install --locked cargo-generate-rpm

WORKDIR /work
