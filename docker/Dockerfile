FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY projects ./projects
RUN cargo build --locked --release -p kline-web-provider

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home app \
    && mkdir -p /app/data \
    && chown -R app:app /app

WORKDIR /app
COPY --from=builder /build/target/release/kline-web-provider /app/kline-web-provider

ENV SERVER_ADDRESS=0.0.0.0 \
    SERVER_PORT=8000

USER app
EXPOSE 8000
VOLUME ["/app/data"]

ENTRYPOINT ["/app/kline-web-provider"]
CMD ["--destination", "/app/data"]
