# The container image of the server build: `freemkv --features server`, the
# unattended rip daemon + web UI (`freemkv server`). Multi-stage: a static musl
# cargo build, a harvest of mount.nfs4 + busybox, then a scratch final image.
#
# Build from the repository root (the build context is the whole crate):
#
#   docker build -f docker/Dockerfile -t freemkv-library .
#
# docker/Dockerfile.dockerignore keeps target/ and the local .cargo/ path
# overrides out of the context, so the freemkv crates resolve from the git
# tags in Cargo.toml exactly as a clean checkout does.

# Must be >= Cargo.toml's `rust-version` (1.98.1). An older image fails
# resolution outright with an MSRV error.
FROM rust:1.98.1-alpine AS builder
# musl-dev + gcc + make + cmake for mimalloc-sys C build; git for the build
# label's commit stamp.
RUN apk add --no-cache musl-dev gcc make cmake git
WORKDIR /build
COPY . .
# The daemon's release profile (thin LTO, one codegen unit), set here rather
# than in Cargo.toml so the CLI and app release builds are unchanged.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
# --locked: refuse to modify Cargo.lock during build, so a lock that no longer
# matches Cargo.toml fails visibly instead of silently re-resolving.
RUN cargo build --locked --release --features server --bin freemkv

# Harvest stage — pull mount.nfs4 + its deps + busybox-static + applet
# symlinks from an alpine image so the FROM scratch final has a working
# shell for operator triage and a working in-container NFS mount.
# Maintenance: verify via `ldd /sbin/mount.nfs4` after any alpine bump.
FROM alpine:3.20 AS harvest
RUN apk add --no-cache nfs-utils busybox-static \
 && mkdir -p /out/sbin /out/lib /out/usr/lib /out/bin /out/etc \
 && cp /sbin/mount.nfs4              /out/sbin/ \
 && cp /sbin/mount.nfs               /out/sbin/ \
 && cp /lib/ld-musl-x86_64.so.1      /out/lib/ \
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
 && cp -r /etc/services              /out/etc/services 2>/dev/null || true \
 && cp -r /etc/nsswitch.conf         /out/etc/nsswitch.conf 2>/dev/null || true

FROM scratch
COPY --from=harvest /out/ /
# Pin perms explicitly on both COPYs; the bootstrap's udev rule runs
# /usr/local/bin/udev-trigger.sh on disc insertion.
COPY --from=builder --chmod=0755 /build/target/release/freemkv /usr/local/bin/freemkv
COPY --chmod=0755 docker/udev-trigger.sh /usr/local/bin/udev-trigger.sh

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/freemkv", "server", "--healthcheck"]

