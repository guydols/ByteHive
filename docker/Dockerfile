FROM rust:slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    glib-2.0 \
    libcairo2-dev \
    pkg-config \
    libpango1.0-dev \
    libatk1.0-dev \
    libgtk-3-dev \
    libxdo-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY ../. .
RUN cargo build --release --workspace


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --no-create-home --shell /bin/false bytehive
WORKDIR /app
COPY --from=builder /build/target/release/bytehive /app/bytehive
RUN mkdir -p /app/config /data/filesync \
    && chown -R bytehive:bytehive /app /data
USER bytehive


# 9000 HTTP portal / dashboard
# 7878 FileSync TCP
EXPOSE 9000 7878

ENTRYPOINT ["/app/bytehive"]
CMD ["--config", "/app/config/config.toml"]
