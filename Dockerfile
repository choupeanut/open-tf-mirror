FROM rust:1-slim-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin appuser

COPY --from=builder /app/target/release/open-tf-mirror /usr/local/bin/open-tf-mirror
RUN ln -s /usr/local/bin/open-tf-mirror /usr/local/bin/hermitcrab

USER appuser

ENTRYPOINT ["/usr/local/bin/open-tf-mirror"]
