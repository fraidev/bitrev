# Multi-stage image for `bitrev serve`.
# Inside the container the daemon binds 0.0.0.0:8080 via BITREV_SERVER_*.
# RUST_LOG selects tracing verbosity (default info).
# TLS terminates at a reverse proxy. State and downloads live on /data.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --release --bin bitrev

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /data bitrev
COPY --from=build /src/target/release/bitrev /usr/local/bin/bitrev
ENV BITREV_SERVER_HOST=0.0.0.0 \
    BITREV_SERVER_PORT=8080 \
    BITREV_STATE_DIR=/data \
    BITREV_DOWNLOAD_DIR=/data/downloads \
    RUST_LOG=info
VOLUME /data
EXPOSE 8080
USER bitrev
WORKDIR /data
ENTRYPOINT ["bitrev", "serve"]
