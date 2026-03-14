# Builder stage
FROM rust:1.87-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release -p trondb-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/trondb-server /usr/local/bin/trondb-server

ENV TRONDB_DATA_DIR=/data/trondb
VOLUME /data/trondb

EXPOSE 9400

ENTRYPOINT ["trondb-server"]
