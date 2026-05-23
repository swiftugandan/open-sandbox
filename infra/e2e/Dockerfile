FROM rust:1.88-slim-trixie AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        protobuf-compiler pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY proto proto
RUN cargo build --release --bin open-sandbox

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/open-sandbox /usr/local/bin/
ENTRYPOINT ["open-sandbox"]
