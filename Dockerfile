# Multi-stage build for mr-bridge
# Stage 1: Build the application
FROM rust:1.92-slim AS builder

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build release binary
RUN cargo build --release && \
    strip /build/target/release/mr-bridge

# Stage 2: Runtime image
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -u 1000 -s /bin/false mrbridge && \
    mkdir -p /app/config && \
    chown -R mrbridge:mrbridge /app

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/mr-bridge /usr/local/bin/mr-bridge

# Copy example configs for reference
COPY config.example.toml /app/config/
COPY config.example.json /app/config/

# Switch to non-root user
USER mrbridge

# Set environment defaults
ENV RUST_LOG=info

# Health check placeholder (can be extended if you add HTTP health endpoint)
# HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
#   CMD pgrep -x mr-bridge || exit 1

ENTRYPOINT ["/usr/local/bin/mr-bridge"]
CMD ["--help"]
