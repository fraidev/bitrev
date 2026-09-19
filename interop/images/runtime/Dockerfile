ARG BASE=debian:bookworm-slim
FROM ${BASE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && (apt-get install -y --no-install-recommends libssl3 \
        || apt-get install -y --no-install-recommends libssl3t64) \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/usr/local/bin/bitrev-interop"]
