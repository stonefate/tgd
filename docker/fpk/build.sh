#!/bin/sh
# 交叉编译 linux/amd64 tgd-server（Web UI rust-embed），再打飞牛离线 .fpk。
set -e
cd "$(dirname "$0")/../.."

FPK_DIR="docker/fpk"
OUT_DIR="${FPK_OUT_DIR:-dist-fpk}"
BIN="$FPK_DIR/app/bin/tgd-server"
FNPACK_VER="${FNPACK_VER:-1.2.3}"
RUST_IMAGE="${TGD_FPK_RUST_IMAGE:-rust:1-bookworm}"

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

build_linux_bin() {
	echo "构建前端 (TGD_WEB_BASE=/app/tgd) ..."
	TGD_WEB_BASE=/app/tgd pnpm build
	# rust-embed 在编译期读 ../build；碰一下源文件避免沿用旧嵌入。
	touch src-tauri/src/server.rs

	echo "交叉编译 linux/amd64 tgd-server ($RUST_IMAGE) ..."
	# 不要 bash -l：登录壳会丢掉镜像 PATH，cargo 找不到。
	docker run --rm --platform linux/amd64 \
		-v "$(pwd)":/workspace \
		-v tgd-cargo-registry:/usr/local/cargo/registry \
		-v tgd-cargo-git:/usr/local/cargo/git \
		-w /workspace/src-tauri \
		-e CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-Wl,--allow-multiple-definition" \
		"$RUST_IMAGE" \
		cargo build --profile docker --bin tgd-server --no-default-features --features server --target-dir /workspace/src-tauri/target_linux

	src="src-tauri/target_linux/docker/tgd-server"
	[ -f "$src" ] || fail "交叉编译没有产出 $src"
	mkdir -p "$(dirname "$BIN")"
	cp "$src" "$BIN"
	chmod +x "$BIN"
}

if [ "${TGD_FPK_SKIP_BIN:-}" != "1" ]; then
	build_linux_bin
fi

if [ ! -f "$BIN" ]; then
	fail "缺少 $BIN。请先跑完整 pnpm fpk:build，或去掉 TGD_FPK_SKIP_BIN。"
fi

chmod +x "$FPK_DIR"/cmd/*
find "$FPK_DIR" -name ".DS_Store" -delete 2>/dev/null || true

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
