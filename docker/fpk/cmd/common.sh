#!/bin/bash
# 生命周期脚本共用：API 凭据、下载目录、启停原生进程。不要打日志输出 hash。

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${PATH}"

tgd_fail() {
	echo "$1" > "${TRIM_TEMP_LOGFILE}"
	exit 1
}

tgd_unquote() {
	local v="$1"
	v="${v#"${v%%[![:space:]]*}"}"
	v="${v%"${v##*[![:space:]]}"}"
	v="${v#$'\r'}"
	v="${v%$'\r'}"
	if [ "${#v}" -ge 2 ]; then
		case "$v" in
		\"*\") v="${v#\"}"; v="${v%\"}" ;;
		\'*\') v="${v#\'}"; v="${v%\'}" ;;
		esac
		v="${v#"${v%%[![:space:]]*}"}"
		v="${v%"${v##*[![:space:]]}"}"
	fi
	printf '%s' "$v"
}

tgd_env_file() {
	printf '%s\n' "${TRIM_PKGETC}/telegram.env"
}

tgd_read_env_value() {
	local file="$1"
	local key="$2"
	local line
	[ -f "$file" ] || return 0
	line=$(grep -E "^${key}=" "$file" 2>/dev/null | tail -n 1) || true
	[ -n "$line" ] || return 0
	tgd_unquote "${line#*=}"
}

# 空文件不含 TELEGRAM_API_ID=，避免空字符串被当成非法数字。
tgd_touch_env() {
	mkdir -p "${TRIM_PKGETC}"
	if [ ! -f "$(tgd_env_file)" ]; then
		umask 077
		: > "$(tgd_env_file)"
	fi
}

tgd_write_env() {
	local id hash existing
	id=$(tgd_unquote "$1")
	hash=$(tgd_unquote "$2")
	existing="$(tgd_env_file)"
	if [ -z "$id" ]; then
		id=$(tgd_read_env_value "$existing" TELEGRAM_API_ID)
	fi
	if [ -z "$hash" ]; then
		hash=$(tgd_read_env_value "$existing" TELEGRAM_API_HASH)
	fi
	if [ -z "$id" ] || [ -z "$hash" ]; then
		tgd_fail "请填写 Telegram API ID 和 API Hash。"
	fi
	case "$id" in
	''|*[!0-9]*)
		tgd_fail "API ID 必须是数字。"
		;;
	esac
	mkdir -p "${TRIM_PKGETC}"
	umask 077
	printf 'TELEGRAM_API_ID=%s\nTELEGRAM_API_HASH=%s\n' "$id" "$hash" > "$existing"
}

tgd_migrate_legacy_env() {
	local dest src id hash
	dest="$(tgd_env_file)"
	[ -f "$dest" ] && grep -qE '^TELEGRAM_API_ID=[0-9]+' "$dest" 2>/dev/null && return 0
	src="${TRIM_APPDEST}/docker/.env"
	[ -f "$src" ] || return 0
	id=$(tgd_read_env_value "$src" TELEGRAM_API_ID)
	hash=$(tgd_read_env_value "$src" TELEGRAM_API_HASH)
	if [ -n "$id" ] && [ -n "$hash" ]; then
		tgd_write_env "$id" "$hash"
	fi
}

tgd_restore_env() {
	tgd_migrate_legacy_env
	if [ ! -f "$(tgd_env_file)" ]; then
		tgd_fail "缺少 Telegram API 配置，请在应用设置里重新填写。"
	fi
}

tgd_env_ready() {
	local id
	id=$(tgd_read_env_value "$(tgd_env_file)" TELEGRAM_API_ID)
	case "$id" in
	''|*[!0-9]*) return 1 ;;
	esac
	return 0
}

tgd_runtime_file() {
	printf '%s\n' "${TRIM_PKGETC}/runtime.env"
}

tgd_split_colon() {
	local rest="$1"
	local part
	while [ -n "$rest" ]; do
		part="${rest%%:*}"
		if [ "$part" = "$rest" ]; then
			rest=
		else
			rest="${rest#*:}"
		fi
		[ -n "$part" ] && printf '%s\n' "$part"
	done
}

tgd_pick_share() {
	local suffix="$1"
	local p
	while IFS= read -r p; do
		[ -n "$p" ] || continue
		case "$p" in
		*"$suffix") printf '%s\n' "$p"; return 0 ;;
		esac
	done <<EOF
$(tgd_split_colon "${TRIM_DATA_SHARE_PATHS:-}")
EOF
	printf '%s\n' "/var/apps/${TRIM_APPNAME:-tgd}/shares/tgd/${suffix#/}"
}

tgd_default_data_dir() {
	tgd_pick_share "/data"
}

tgd_default_download_dir() {
	tgd_pick_share "/downloads"
}

tgd_ensure_runtime_env() {
	local f
	f="$(tgd_runtime_file)"
	mkdir -p "${TRIM_PKGETC}"
	if [ ! -f "$f" ]; then
		umask 077
		printf 'TGD_DOWNLOAD_DIR=%s\n' "$(tgd_default_download_dir)" > "$f"
	fi
}

tgd_is_safe_abs_path() {
	local p="$1"
	local rest part
	case "$p" in
	/*) ;;
	*) return 1 ;;
	esac
	case "$p" in
	*$'\n'* | *$'\r'* | *':'*) return 1 ;;
	esac
	rest="${p#/}"
	while [ -n "$rest" ]; do
		part="${rest%%/*}"
		if [ "$part" = "$rest" ]; then
			rest=
		else
			rest="${rest#*/}"
		fi
		[ "$part" = ".." ] && return 1
		[ "$part" = "." ] && return 1
	done
	case "$p" in
	/ | /bin | /bin/* | /boot | /boot/* | /dev | /dev/* | /etc | /etc/* | /proc | /proc/* | /sys | /sys/* | /root | /root/*)
		return 1
		;;
	esac
	return 0
}

tgd_is_default_download_host() {
	local p="$1"
	local share
	[ "$p" = "/downloads" ] && return 0
	[ "$p" = "/var/apps/${TRIM_APPNAME:-tgd}/shares/tgd/downloads" ] && return 0
	[ "$p" = "/var/apps/${TRIM_APPNAME:-tgd}/share/downloads" ] && return 0
	while IFS= read -r share; do
		[ "$p" = "$share" ] || continue
		case "$share" in
		*/tgd/downloads | */downloads) return 0 ;;
		esac
	done <<EOF
$(tgd_split_colon "${TRIM_DATA_SHARE_PATHS:-}")
EOF
	return 1
}

tgd_path_allowed() {
	local want="$1"
	local base
	[ "$want" = "/downloads" ] && return 0
	while IFS= read -r base; do
		[ -n "$base" ] || continue
		tgd_is_safe_abs_path "$base" || continue
		[ "$want" = "$base" ] && return 0
		case "$want" in
		"$base"/*) return 0 ;;
		esac
	done <<EOF
$(tgd_split_colon "${TRIM_DATA_ACCESSIBLE_PATHS:-}")
$(tgd_split_colon "${TRIM_DATA_SHARE_PATHS:-}")
EOF
	return 1
}

tgd_write_runtime_dir() {
	local dir="$1"
	mkdir -p "${TRIM_PKGETC}"
	umask 077
	printf 'TGD_DOWNLOAD_DIR=%s\n' "$dir" > "$(tgd_runtime_file)"
}

tgd_apply_download_config() {
	local want existing resolved
	tgd_ensure_runtime_env
	want=$(tgd_unquote "$1")
	existing=$(tgd_read_env_value "$(tgd_runtime_file)" TGD_DOWNLOAD_DIR)
	if [ -z "$want" ]; then
		want="$existing"
	fi
	if [ -z "$want" ] || tgd_is_default_download_host "$want"; then
		resolved="$(tgd_default_download_dir)"
	else
		tgd_is_safe_abs_path "$want" || tgd_fail "下载目录必须是绝对路径，不要包含 .. 或冒号。"
		tgd_path_allowed "$want" || tgd_fail "请先在「访问权限」添加该文件夹并保存，再把它的完整路径填到下载目录。"
		resolved="$want"
	fi
	tgd_write_runtime_dir "$resolved"
}

# 从旧 Docker 包升级时清掉残留容器，避免占 8787。
tgd_drop_legacy_docker() {
	if command -v docker >/dev/null 2>&1; then
		docker rm -f tgd >/dev/null 2>&1 || true
	elif [ -x /usr/bin/docker ]; then
		/usr/bin/docker rm -f tgd >/dev/null 2>&1 || true
	fi
}

tgd_load_process_env() {
	tgd_migrate_legacy_env
	tgd_touch_env
	tgd_ensure_runtime_env
	set -a
	# shellcheck disable=SC1090
	[ -f "$(tgd_env_file)" ] && . "$(tgd_env_file)"
	# shellcheck disable=SC1090
	[ -f "$(tgd_runtime_file)" ] && . "$(tgd_runtime_file)"
	set +a
	export TGD_DATA_DIR="$(tgd_default_data_dir)"
	local dir="${TGD_DOWNLOAD_DIR:-}"
	if [ -z "$dir" ] || tgd_is_default_download_host "$dir"; then
		dir="$(tgd_default_download_dir)"
	fi
	export TGD_DOWNLOAD_DIR="$dir"
	export TGD_LISTEN="${TGD_LISTEN:-0.0.0.0:8787}"
	export TGD_LISTEN_SOCKET="${TRIM_APPDEST}/app.sock"
	export TGD_WEB_BASE="${TGD_WEB_BASE:-/app/tgd}"
	export TZ="${TZ:-Asia/Shanghai}"
	unset TGD_WEB_DIR
	mkdir -p "$TGD_DATA_DIR" "$TGD_DOWNLOAD_DIR" "${TRIM_PKGVAR}" "${TRIM_APPDEST}"
}

tgd_restart() {
	"$(dirname "$0")/main" stop
	if ! "$(dirname "$0")/main" start; then
		tgd_fail "启动 tgd-server 失败，请看应用日志。"
	fi
}
