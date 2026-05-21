FROM rust:slim AS builder
RUN apt-get update && apt-get install -y protobuf-compiler pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY proto proto
RUN cargo build --release --bin open-sandbox

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/open-sandbox /usr/local/bin/
ENTRYPOINT ["open-sandbox"]
