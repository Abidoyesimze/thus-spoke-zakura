FROM node:24-alpine AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json* ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1.98-bookworm AS rust
WORKDIR /src
ARG RUST_PROFILE=release
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY crates/ crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --profile "$RUST_PROFILE" --locked -p ths-server && \
    if [ "$RUST_PROFILE" = dev ]; then target_dir=debug; else target_dir="$RUST_PROFILE"; fi && \
    cp "/src/target/$target_dir/ths-server" /tmp/ths-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=rust /tmp/ths-server /usr/local/bin/ths-server
COPY --from=web /src/web/dist /opt/ths/web
ENV THS_WEB_DIR=/opt/ths/web
ENTRYPOINT ["ths-server"]
