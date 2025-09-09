# ──────────────────────────────
# Builder
# ──────────────────────────────
FROM rustlang/rust:nightly-bullseye AS builder

WORKDIR /app

# Pre-build dependency layer for caching
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs
RUN cargo build --release || true

# Now copy the actual sources and force rebuild
COPY . .
RUN touch src/main.rs && cargo build --release

# ──────────────────────────────
# Runtime
# ──────────────────────────────
FROM debian:bullseye-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/claw /usr/local/bin/claw

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/claw"]
