# ---- Builder: compile the Rust project (release) ----
FROM rust:alpine AS builder
WORKDIR /app

# Build deps (musl toolchain bits)
RUN apk add --no-cache \
  build-base \
  pkgconfig \
  musl-dev

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch
COPY src src
RUN cargo build --release

# ---- Runner option A: Plain Alpine (no Node) ----
FROM alpine:latest AS runner
WORKDIR /app

# Common runtime deps (adjust as needed)
RUN apk add --no-cache ca-certificates git tar zstd

RUN addgroup -S app && adduser -S app -G app
# Fix /app dir permissions for the app user
RUN chown -R app:app /app

COPY --from=builder /app/target/release/github_backup /usr/local/bin/github_backup

# set `RUST_LOG` to `info` by default, can be overridden by setting the environment variable when running the container
ENV RUST_LOG=info

USER app
ENTRYPOINT ["github_backup"]
