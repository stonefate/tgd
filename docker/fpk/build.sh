#!/bin/sh
# 把已构建的 linux/amd64 镜像打进飞牛离线 .fpk（自用手动安装）。
set -e
cd "$(dirname "$0")/../.."

IMAGE="${IMAGE:-tgd:0.1.18}"
FPK_DIR="docker/fpk"
OUT_DIR="${FPK_OUT_DIR:-dist-fpk}"
TAR="$FPK_DIR/app/docker/tgd.tar"
FNPACK_VER="${FNPACK_VER:-1.2.3}"

fail() {
	echo "$1" >&2
	exit 1
}

ensure_fnpack() {
	if [ -n "$FNPACK_BIN" ] && [ -x "$FNPACK_BIN" ]; then
		return
	fi
	if command -v fnpack >/dev/null 2>&1; then
		FNPACK_BIN=$(command -v fnpack)
		return
	fi
	os=$(uname -s)
	arch=$(uname -m)
	case "$os-$arch" in
	Darwin-arm64) pack=darwin-arm64 ;;
	Darwin-x86_64) pack=darwin-amd64 ;;
	Linux-x86_64 | Linux-amd64) pack=linux-amd64 ;;
	Linux-aarch64 | Linux-arm64) pack=linux-arm64 ;;
	*) fail "不支持的系统: $os $arch，请设置 FNPACK_BIN" ;;
	esac
	cache="${HOME}/.cache/tgd-fnpack"
	mkdir -p "$cache"
	FNPACK_BIN="$cache/fnpack"
	if [ ! -x "$FNPACK_BIN" ]; then
		echo "下载 fnpack ${FNPACK_VER} ($pack) ..."
		curl -fsSL -o "$FNPACK_BIN" "https://static2.fnnas.com/fnpack/fnpack-${FNPACK_VER}-${pack}"
		chmod +x "$FNPACK_BIN"
	fi
}

export_docker_archive() {
	# Docker Desktop 的 docker save 是 OCI layout，飞牛偏旧的 docker load 吃不下。
	mkdir -p "$(dirname "$TAR")"
	oci="${TAR}.oci"
	echo "导出镜像 $IMAGE -> $TAR"
	docker save -o "$oci" "$IMAGE"
	python3 docker/oci-to-docker-archive.py "$oci" "$TAR"
	rm -f "$oci"
}

if [ "${TGD_FPK_SKIP_IMAGE:-}" != "1" ]; then
	# Web UI 打在镜像里。只 fnpack 会把本机已有的旧镜像打进去，设置页还是「由 Compose 卷挂载」。
	echo "构建镜像 $IMAGE ..."
	IMAGE="$IMAGE" sh docker/build.sh
	export_docker_archive
fi

if [ ! -f "$TAR" ]; then
	fail "缺少 $TAR。请先 pnpm docker:build，或去掉 TGD_FPK_SKIP_IMAGE。"
fi

chmod +x "$FPK_DIR"/cmd/*

ensure_fnpack
echo "fnpack build ($FNPACK_BIN) ..."
"$FNPACK_BIN" build --directory "$FPK_DIR"

mkdir -p "$OUT_DIR"
found=0
for f in "$FPK_DIR"/*.fpk ./*.fpk; do
	[ -f "$f" ] || continue
	mv "$f" "$OUT_DIR/"
	found=1
done
[ "$found" = 1 ] || fail "fnpack 没有产出 .fpk"

ls -lh "$OUT_DIR"/*.fpk
