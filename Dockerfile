# Build stage
FROM rust:1.75 as builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source
COPY src ./src
COPY examples ./examples
COPY benches ./benches

# Build release binary
RUN cargo build --release --example axum_server --features axum-integration

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/examples/axum_server /app/server

# Create directory for SQLite database
RUN mkdir -p /app/data

EXPOSE 3000

CMD ["./server"]
