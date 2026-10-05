# The container image of the server build: `freemkv --features server`, the
# unattended rip daemon + web UI (`freemkv server`). Multi-stage: a static musl
# cargo build, a harvest of mount.nfs4 + busybox, then a scratch final image.
# Published multi-arch (amd64, arm64, arm/v7) by .github/workflows/server-image.yml.
#
# Build from the repository root (the build context is the whole crate):
#
#   docker build -f docker/Dockerfile -t freemkv-library .
#
# docker/Dockerfile.dockerignore keeps target/ and the local .cargo/ path
# overrides out of the context, so the freemkv crates resolve from the git
# tags in Cargo.toml exactly as a clean checkout does.

# One image per CPU: linux/amd64, linux/arm64 and linux/arm/v7 (Raspberry Pi
# 32-bit). `FROM build-${TARGETARCH}` picks the builder, so this needs BuildKit
# (the default since Docker 23); `docker buildx build --platform ...` builds any.
ARG TARGETARCH

# amd64 and arm64 build natively in the official Rust image (CI builds arm64 on
# an arm64 runner). Must be >= Cargo.toml's `rust-version` (1.98.1). An older
# image fails resolution outright with an MSRV error.
FROM rust:1.98.1-alpine AS build-native
# musl-dev + gcc + make + cmake for mimalloc-sys C build; git for the sibling
# clones of a SIBLING_REF build.
RUN apk add --no-cache musl-dev gcc make cmake git
WORKDIR /build
COPY . .
# The daemon's release profile (thin LTO, one codegen unit), set here rather
# than in Cargo.toml so the CLI and app release builds are unchanged.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
# SIBLING_REF=dev builds against the sibling crates' branch tips (see build-server.sh).
ARG SIBLING_REF=
# Version label shown by the server (e.g. 1.7.7-dev.<sha>); empty = the crate version.
ARG AUTORIP_BUILD_LABEL=
RUN sh docker/build-server.sh
FROM build-native AS build-amd64
FROM build-native AS build-arm64

# arm/v7 has no rust:alpine image, so it cross-compiles on amd64 in the same
# pinned cross image the armv7 CLI release binary uses (its env already points
# cargo and cc-rs at arm-linux-musleabihf-gcc). Only the harvest stage below
# runs under emulation, and that is just apk.
FROM --platform=linux/amd64 ghcr.io/cross-rs/armv7-unknown-linux-musleabihf:0.2.5@sha256:b4f481f30ceca34730a5723f717396d762d94f6eddd7e1cf7c7862d1377ff98c AS build-arm
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
# rustup-init pinned + hash-checked; it then installs Cargo.toml's exact rust-version.
RUN curl -fsSLo /tmp/rustup-init https://static.rust-lang.org/rustup/archive/1.29.1/x86_64-unknown-linux-gnu/rustup-init \
 && echo 'dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71  /tmp/rustup-init' | sha256sum -c - \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --profile minimal --default-toolchain none --no-modify-path \
 && rm /tmp/rustup-init
WORKDIR /build
COPY . .
RUN tc=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml | head -1) \
 && rustup toolchain install "$tc" --profile minimal --target armv7-unknown-linux-musleabihf \
 && rustup default "$tc"
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
ARG SIBLING_REF=
ARG AUTORIP_BUILD_LABEL=
RUN sh docker/build-server.sh armv7-unknown-linux-musleabihf

FROM build-${TARGETARCH} AS builder

# Harvest stage — pull mount.nfs4 + its deps + busybox-static + applet
# symlinks from an alpine image so the FROM scratch final has a working
# shell for operator triage and a working in-container NFS mount.
# Maintenance: verify via `ldd /sbin/mount.nfs4` after any alpine bump.
FROM alpine:3.20 AS harvest
RUN apk add --no-cache nfs-utils busybox-static \
 && mkdir -p /out/sbin /out/lib /out/usr/lib /out/bin /out/etc \
 && cp /sbin/mount.nfs4              /out/sbin/ \
 && cp /sbin/mount.nfs               /out/sbin/ \
 && cp /lib/ld-musl-*.so.1           /out/lib/ \
 && cp /lib/libmount.so.1            /out/lib/ \
 && cp /lib/libblkid.so.1            /out/lib/ \
 && cp /lib/libcom_err.so.2          /out/lib/ \
 && cp /usr/lib/libtirpc.so.3        /out/usr/lib/ \
 && cp /usr/lib/libkeyutils.so.1     /out/usr/lib/ \
 && cp /usr/lib/libgssapi_krb5.so.2  /out/usr/lib/ \
 && cp /usr/lib/libkrb5.so.3         /out/usr/lib/ \
 && cp /usr/lib/libk5crypto.so.3     /out/usr/lib/ \
 && cp /usr/lib/libkrb5support.so.0  /out/usr/lib/ \
 && cp /usr/lib/libeconf.so.0        /out/usr/lib/ \
 && cp /bin/busybox.static           /out/bin/busybox \
 && for app in sh mount umount mountpoint ls cat env wget mkdir chown ln rm id ps grep less head tail; do \
        ln -sf /bin/busybox /out/bin/$app; \
    done \
 && mkdir -p /out/usr/local/bin && ln -s freemkv /out/usr/local/bin/autorip \
 && cp -r /etc/services              /out/etc/services 2>/dev/null || true \
 && cp -r /etc/nsswitch.conf         /out/etc/nsswitch.conf 2>/dev/null || true
# ffmpeg for the Library's deep audit (off by default), with every library it links.
RUN apk add --no-cache ffmpeg \
 && cp /usr/bin/ffmpeg /out/usr/local/bin/ffmpeg \
 && ldd /usr/bin/ffmpeg | awk '/=>/ {print $3}' | while read -r lib; do \
        mkdir -p "/out$(dirname "$lib")" && cp -L "$lib" "/out$lib"; \
    done

FROM scratch
LABEL org.opencontainers.image.source=https://github.com/freemkv/freemkv
COPY --from=harvest /out/ /
# Pin perms explicitly on both COPYs; the bootstrap's udev rule runs
# /usr/local/bin/udev-trigger.sh on disc insertion.
COPY --from=builder --chmod=0755 /build/out/freemkv /usr/local/bin/freemkv
COPY --chmod=0755 docker/udev-trigger.sh /usr/local/bin/udev-trigger.sh

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/freemkv", "server", "--healthcheck"]

# --bootstrap mounts NFS / installs the udev rule, then runs the daemon.
ENTRYPOINT ["/usr/local/bin/freemkv", "server", "--bootstrap"]

