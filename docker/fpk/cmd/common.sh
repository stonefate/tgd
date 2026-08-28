#!/bin/bash
# 生命周期脚本共用：离线镜像导入、API 凭据写入。不要打日志输出 hash。

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${PATH}"

tgd_fail() {
	echo "$1" > "${TRIM_TEMP_LOGFILE}"
	exit 1
}

tgd_docker() {
	if command -v docker >/dev/null 2>&1; then
		docker "$@"
	elif [ -x /usr/bin/docker ]; then
		/usr/bin/docker "$@"
	else
		tgd_fail "找不到 docker 命令。"
	fi
}

tgd_find_tar() {
	local p root found
	for p in \
		"${TRIM_APPDEST}/docker/tgd.tar" \
		"${TRIM_PKGINST_TEMP_DIR}/app/docker/tgd.tar" \
		"${TRIM_TEMP_TPKFILE}/app/docker/tgd.tar" \
		"$(dirname "$0")/../app/docker/tgd.tar"
	do
		[ -n "$p" ] && [ -f "$p" ] && printf '%s\n' "$p" && return 0
	done
	for root in "$TRIM_APPDEST" "$TRIM_PKGINST_TEMP_DIR" "$TRIM_TEMP_TPKFILE" "$TRIM_PKGTMP"; do
		[ -n "$root" ] && [ -d "$root" ] || continue
		found=$(find "$root" -name 'tgd.tar' -type f 2>/dev/null | head -n 1)
		[ -n "$found" ] && printf '%s\n' "$found" && return 0
	done
	return 1
}

tgd_load_image() {
	local tar err
	tar=$(tgd_find_tar)
	if [ -n "$tar" ] && [ -s "$tar" ]; then
		err=$(tgd_docker load -i "$tar" 2>&1) || tgd_fail "导入离线镜像失败：${err}"
		rm -f "${TRIM_APPDEST}/docker/tgd.tar"
		return 0
	fi
	if tgd_docker image inspect "${TGD_IMAGE:-tgd:0.1.17}" >/dev/null 2>&1; then
		return 0
	fi
	tgd_fail "离线镜像包缺失，无法继续。"
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

# compose 解析 env_file 时文件必须存在；空文件不含 TELEGRAM_API_ID=，避免空字符串被当成非法数字。
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

tgd_ensure_runtime_env() {
	local f
	f="$(tgd_runtime_file)"
	mkdir -p "${TRIM_PKGETC}"
	if [ ! -f "$f" ]; then
		umask 077
		printf 'TGD_DOWNLOAD_DIR=/downloads\n' > "$f"
	fi
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

tgd_yaml_quote() {
	local s="$1"
	s="${s//\\/\\\\}"
	s="${s//\"/\\\"}"
	printf '"%s"' "$s"
}

tgd_write_runtime_dir() {
	local dir="$1"
	mkdir -p "${TRIM_PKGETC}"
	umask 077
	printf 'TGD_DOWNLOAD_DIR=%s\n' "$dir" > "$(tgd_runtime_file)"
}

tgd_host_volume_roots() {
	local n
	for n in 1 2 3 4 5 6; do
		[ -d "/vol${n}" ] && printf '%s\n' "/vol${n}"
	done
}

# 飞牛应用中心常用 `compose -f docker-compose.yaml`，不会自动读 override。
# 把额外卷写进主 compose 的标记段，保证授权路径在容器里可见。
tgd_write_compose_override() {
	local dir="${TRIM_APPDEST}/docker"
	local compose="$dir/docker-compose.yaml"
	local override="$dir/docker-compose.override.yaml"
	local block="$dir/.tgd-extra-volumes"
	local extra count=0 dup existing
	local -a extras=()
	mkdir -p "$dir"

	while IFS= read -r extra; do
		[ -n "$extra" ] || continue
		tgd_is_safe_abs_path "$extra" || continue
		[ "$extra" = "/data" ] && continue
		[ "$extra" = "/downloads" ] && continue
		[ -d "$extra" ] || continue
		dup=0
		for existing in "${extras[@]}"; do
			[ "$existing" = "$extra" ] && dup=1 && break
		done
		[ "$dup" = 1 ] && continue
		extras+=("$extra")
		count=$((count + 1))
		[ "$count" -ge 16 ] && break
	done <<EOF
$(tgd_host_volume_roots)
$(tgd_split_colon "${TRIM_DATA_ACCESSIBLE_PATHS:-}")
$(tgd_split_colon "${TRIM_DATA_SHARE_PATHS:-}")
EOF

	: > "$block"
	for extra in "${extras[@]}"; do
		printf '      - %s\n' "$(tgd_yaml_quote "$extra:$extra")" >> "$block"
	done

	if [ -f "$compose" ] && grep -q 'tgd-extra-volumes-start' "$compose"; then
		awk -v block="$block" '
			/tgd-extra-volumes-start/ {
				print
				while ((getline line < block) > 0) print line
				close(block)
				skip=1
				next
			}
			/tgd-extra-volumes-end/ { skip=0 }
			skip { next }
			{ print }
		' "$compose" > "$compose.tmp" && mv "$compose.tmp" "$compose"
	fi

	if [ "${#extras[@]}" -eq 0 ]; then
		rm -f "$override"
	else
		{
			printf 'services:\n  tgd:\n    volumes:\n'
			cat "$block"
		} > "$override"
	fi
	rm -f "$block"
	if [ -n "${TRIM_PKGETC:-}" ]; then
		mkdir -p "${TRIM_PKGETC}"
		{
			echo "ACCESSIBLE=${TRIM_DATA_ACCESSIBLE_PATHS:-}"
			echo "SHARE=${TRIM_DATA_SHARE_PATHS:-}"
			printf '%s\n' "${extras[@]}"
		} > "${TRIM_PKGETC}/extra-volumes.list"
	fi
	tgd_write_compose_user "$compose"
}

tgd_share_owner() {
	local p
	for p in \
		"/var/apps/${TRIM_APPNAME:-tgd}/shares/tgd/downloads" \
		"/var/apps/${TRIM_APPNAME:-tgd}/shares/tgd/data" \
		"/var/apps/${TRIM_APPNAME:-tgd}/shares/tgd"
	do
		[ -e "$p" ] || continue
		stat -c '%u:%g' "$p" 2>/dev/null && return 0
	done
	return 1
}

# 容器用户对齐共享目录所有者，避免下载文件变成 root。uid 0 则不写 user（保持 root）。
tgd_write_compose_user() {
	local compose="$1"
	local owner uid
	[ -f "$compose" ] || return 0
	grep -q 'tgd-user-start' "$compose" || return 0
	owner=$(tgd_share_owner) || owner=
	uid="${owner%%:*}"
	case "$uid" in
	''|0) owner= ;;
	esac
	awk -v user="$owner" '
		/tgd-user-start/ {
			print
			if (user != "") print "    user: \"" user "\""
			skip=1
			next
		}
		/tgd-user-end/ { skip=0 }
		skip { next }
		{ print }
	' "$compose" > "$compose.tmp" && mv "$compose.tmp" "$compose"
}

# 非 root 容器要能在 /fnos 写下 app.sock。只改目录属主，不递归。
tgd_prepare_socket_dir() {
	local dest owner
	dest="${TRIM_APPDEST:-}"
	[ -n "$dest" ] || return 0
	mkdir -p "$dest"
	owner=$(tgd_share_owner) || return 0
	case "${owner%%:*}" in
	''|0) return 0 ;;
	esac
	chown "$owner" "$dest" 2>/dev/null || true
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
		resolved=/downloads
	else
		tgd_is_safe_abs_path "$want" || tgd_fail "下载目录必须是绝对路径，不要包含 .. 或冒号。"
		tgd_path_allowed "$want" || tgd_fail "请先在「访问权限」添加该文件夹并保存，再把它的完整路径填到下载目录。"
		resolved="$want"
	fi
	tgd_write_runtime_dir "$resolved"
	tgd_write_compose_override
}

tgd_container_name() {
	local compose="$1"
	local name
	name=$(grep -E '^[[:space:]]*container_name:' "$compose" 2>/dev/null | head -n 1)
	name="${name#*:}"
	name=$(tgd_unquote "$name")
	printf '%s\n' "${name:-tgd}"
}

# 应用中心 compose 项目名是 appname（网络 tgd_default），不要用 docker 目录名当项目名。
tgd_compose_project() {
	printf '%s\n' "${TRIM_APPNAME:-tgd}"
}

# 应用中心可能已经 compose up 过，项目名又和 --project-directory 不一致，
# --force-recreate 清不掉那个 /tgd，再 up 会 Conflict。
tgd_remove_named_container() {
	local name="$1"
	[ -n "$name" ] || return 0
	tgd_docker rm -f "$name" >/dev/null 2>&1 || true
}

# 飞牛在 install_init 之后就会 compose up。残留 /tgd 必须在那之前清掉。
tgd_drop_stale_container() {
	tgd_remove_named_container "$(tgd_container_name "${TRIM_APPDEST}/docker/docker-compose.yaml")"
}

tgd_compose_up() {
	local dir="${TRIM_APPDEST}/docker"
	local compose="$dir/docker-compose.yaml"
	local override="$dir/docker-compose.override.yaml"
	local project
	project=$(tgd_compose_project)
	[ -f "$compose" ] || return 0
	if tgd_docker compose version >/dev/null 2>&1; then
		if [ -f "$override" ]; then
			tgd_docker compose --project-name "$project" -f "$compose" -f "$override" --project-directory "$dir" up -d --force-recreate
		else
			tgd_docker compose --project-name "$project" -f "$compose" --project-directory "$dir" up -d --force-recreate
		fi
	else
		if [ -f "$override" ]; then
			docker-compose --project-name "$project" -f "$compose" -f "$override" --project-directory "$dir" up -d --force-recreate
		else
			docker-compose --project-name "$project" -f "$compose" --project-directory "$dir" up -d --force-recreate
		fi
	fi
}

# docker restart 不会重读 compose 环境变量 / 新增卷
tgd_recreate() {
	[ -f "${TRIM_APPDEST}/docker/docker-compose.yaml" ] || return 0
	tgd_prepare_socket_dir
	tgd_drop_stale_container
	tgd_compose_up
}
