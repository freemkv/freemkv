# Local-dev Dockerfile. Multi-stage cargo build + harvest + FROM scratch.
#
# CI uses Dockerfile.ci which skips the cargo build by reusing the
# musl-static binary the release `build` job already produced. This
# file is for developers who want to `docker build` against the
# current working tree without an upstream artifact.

# Must be >= Cargo.toml's `rust-version` (1.98) and at least as new as the
# toolchain CI resolves (dtolnay/rust-toolchain@stable in ci.yml / release.yml).
# An older image fails resolution outright with an MSRV error.
FROM rust:1.98-alpine AS builder
# musl-dev + gcc + make + cmake for mimalloc-sys C build.
RUN apk add --no-cache musl-dev gcc make cmake
WORKDIR /build
COPY . .
# --locked: refuse to modify Cargo.lock during build. If the lock-pinned
# libfreemkv version isn't yet on crates.io, hard-fail with a visible
# error instead of silently re-resolving to the previous published
# version (the v0.18.3 incident).
RUN cargo build --locked --release

# Harvest stage — pull mount.nfs4 + its deps + busybox-static + applet
# symlinks from an alpine image so the FROM scratch final has a working
# shell for operator triage and a working in-container NFS mount.
# Maintenance: keep this list in sync with `Dockerfile.ci` — verify
# via `ldd /sbin/mount.nfs4` after any alpine bump.
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
# --chmod=0755 on the host-context COPY for udev-trigger.sh
# (cargo-built binary already has +x from the builder stage but
# pin perms explicitly to match Dockerfile.ci).
COPY --from=builder --chmod=0755 /build/target/release/autorip /usr/local/bin/autorip
COPY --chmod=0755 udev-trigger.sh /usr/local/bin/udev-trigger.sh

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/autorip", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/autorip", "--bootstrap"]
