# Builder stage (trixie for glibc 2.41 — ort_sys ONNX C++ needs C23 functions)
FROM rust:1.88-trixie AS builder

RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release -p trondb-server

# Runtime stage (must match builder glibc)
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/trondb-server /usr/local/bin/trondb-server

# ONNX embedding models
COPY models/embeddings/model.onnx /models/bge-small-en-v1.5/model.onnx
COPY models/embeddings/tokenizer.json /models/bge-small-en-v1.5/tokenizer.json
COPY models/splade/model.onnx /models/splade-pp-en-v1/model.onnx
COPY models/splade/tokenizer.json /models/splade-pp-en-v1/tokenizer.json

ENV TRONDB_DATA_DIR=/data/trondb
VOLUME /data/trondb

EXPOSE 9400

ENTRYPOINT ["trondb-server"]
