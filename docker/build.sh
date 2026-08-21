#!/bin/sh
# 构建飞牛用的 linux/amd64 镜像。兼容 Docker Desktop、Homebrew docker-buildx、旧 docker build。
set -e
cd "$(dirname "$0")/.."

IMAGE="${IMAGE:-tgd:0.1.9}"
PLATFORM="${DOCKER_PLATFORM:-linux/amd64}"

if docker buildx version >/dev/null 2>&1; then
	exec docker buildx build --platform "$PLATFORM" --load -f docker/Dockerfile -t "$IMAGE" .
fi

if command -v docker-buildx >/dev/null 2>&1; then
	exec docker-buildx build --platform "$PLATFORM" --load -f docker/Dockerfile -t "$IMAGE" .
fi

# cache mount 需要 BuildKit；旧 docker build 默认可能没开
exec env DOCKER_BUILDKIT=1 docker build --platform "$PLATFORM" -f docker/Dockerfile -t "$IMAGE" .
