# syntax=docker/dockerfile:1
#
# Nuage DB server image (the database, the console at /admin and the website with its live
# dashboard at /app), for any container host:
#
#   docker build -t nuage .
#   docker run -p 8080:8080 -v nuage-data:/data -e NUAGE_ENCRYPTION_KEY=... -e NUAGE_TOKEN=... -e NUAGE_ADMIN_PASSWORD=... nuage
#
# The browser bundle of the dashboard is committed (crates/nuage-site/pkg) and embedded in
# the binary: no WebAssembly build here.

# The same pinned toolchain as RustConfig: bump both together.
ARG RUST_VERSION=1.99

FROM rust:${RUST_VERSION}-slim-bookworm AS builder
ENV CARGO_TERM_COLOR=never CARGO_NET_RETRY=5
WORKDIR /app
COPY . .
RUN cargo build --release --locked -p nuage --bin nuage --features site

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin nuage \
 && mkdir /data && chown nuage /data
COPY --from=builder /app/target/release/nuage /usr/local/bin/nuage
ENV NUAGE_DATA_DIR=/data PORT=8080
VOLUME /data
USER 10001
EXPOSE 8080
CMD ["nuage"]
