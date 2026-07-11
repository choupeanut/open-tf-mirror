FROM rust:1.88-slim-bookworm@sha256:38bc5a86d998772d4aec2348656ed21438d20fcdce2795b56ca434cf21430d89 AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df AS runtime

RUN printf 'appuser:x:10001:\n' >> /etc/group \
    && printf 'appuser:x:10001:10001:open-tf-mirror:/var/run/open-tf-mirror:/usr/sbin/nologin\n' >> /etc/passwd \
    && install -d -o 10001 -g 10001 \
        /var/run/open-tf-mirror \
        /var/run/open-tf-mirror/providers \
        /var/run/open-tf-mirror/metadata

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /app/target/release/open-tf-mirror /usr/local/bin/open-tf-mirror

USER 10001:10001

ENTRYPOINT ["/usr/local/bin/open-tf-mirror"]
