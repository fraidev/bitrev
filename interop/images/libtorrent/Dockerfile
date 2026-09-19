FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        python3 \
        python3-libtorrent \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /interop
ENTRYPOINT ["python3"]
