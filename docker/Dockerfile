# syntax=docker/dockerfile:1

FROM node:22-bookworm-slim AS frontend
WORKDIR /src
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY . .
ENV TGD_WEB_BASE=/app/tgd
RUN pnpm build

FROM rust:1-bookworm AS backend
WORKDIR /src
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends -o Acquire::Retries=5 \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY src-tauri ./src-tauri
COPY --from=frontend /src/build ./build
# Cargo 只从 cwd 往上找 .cargo/config.toml，--manifest-path 不会读 src-tauri/.cargo。
# 进包目录构建，并显式写 rustflags，否则 rust-lld 会因 sqlx+grammers 双份 sqlite3 链接失败。
WORKDIR /src/src-tauri
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-Wl,--allow-multiple-definition"
# --no-default-features：不编 Tauri/GTK
# --profile docker：opt-level=2、不开 LTO；产物在 target/docker/
# cache：源码变了仍复用 crates 和已编依赖；二进制要从 cache 拷进层
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/src-tauri/target \
    cargo build --profile docker --bin tgd-server --no-default-features --features server \
    && cp /src/src-tauri/target/docker/tgd-server /tgd-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend /tgd-server /usr/local/bin/tgd-server
COPY --from=frontend /src/build /app/web
ENV TGD_WEB_DIR=/app/web \
    TGD_DATA_DIR=/data \
    TGD_DOWNLOAD_DIR=/downloads \
    TGD_LISTEN=0.0.0.0:8787 \
    TGD_WEB_BASE=/app/tgd
EXPOSE 8787
VOLUME ["/data", "/downloads"]
CMD ["tgd-server"]
