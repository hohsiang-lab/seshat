FROM rust:1.94.1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 seshat \
    && useradd --system --uid 10001 --gid 10001 --no-create-home seshat

COPY --from=builder /build/target/release/seshat /usr/local/bin/seshat

USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/seshat"]
