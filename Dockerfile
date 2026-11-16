FROM docker.io/library/rust:1.99-slim-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home readeckobo

WORKDIR /app

COPY --from=builder /app/target/release/readeckobo /app/readeckobo

USER 10001
EXPOSE 8080

ENTRYPOINT ["/app/readeckobo"]
CMD ["serve"]
