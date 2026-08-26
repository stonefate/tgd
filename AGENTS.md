# tgd Agent 约定

Telegram 桌面端：托盘常驻 + 静默抓取/监控。改代码前先对齐范围，只改必要部分。

## 技术栈

| 层 | 选型 |
| --- | --- |
| 桌面 | Tauri 2，identifier `com.tgd.app` |
| 前端 | Svelte 5 + SvelteKit + Vite，`adapter-static` SPA（`ssr = false`） |
| UI | Tailwind CSS v4 + shadcn-svelte（Vega / zinc） |
| 图标 | `@lucide/svelte`（不要再装 lucide-vue-next / lucide-react） |
| 虚拟列表 | `@tanstack/svelte-virtual`（已依赖，消息列表再用） |
| 类型同步 | tauri-specta v2（精确版本见 `src-tauri/Cargo.toml`） |
| Telegram | `grammers-client` 0.10 + `SqliteSession`，user 账号，不是 Bot API |
| 飞牛 | 同一套业务 + axum headless（feature `server`），Compose 自用，浏览器打开 Web UI；窄屏底栏 + 会话列表/详情栈，宽屏仍是侧栏 |

## 目录

```
src/                      # SvelteKit
  lib/bindings.ts         # specta 生成，禁止手改
  lib/api.ts              # 桌面走 bindings，浏览器走 HTTP/SSE
  lib/app-state.svelte.ts # 前端共享状态（连接 / 会话 / 下载）
  lib/components/ui/      # shadcn-svelte
  routes/                 # `/` 会话，`/search` 跨群搜索，`/downloads` 下载，`/settings` 设置
src-tauri/src/
  lib.rs                  # 插件、specta、关窗进托盘
  runtime.rs              # AppCtx / EventHub（桌面与 server 共用）
  service.rs              # 命令业务，Tauri 与 HTTP 共用
  commands.rs             # 导出给前端的命令 / 事件
  server.rs               # axum HTTP/SSE（仅 feature `server`）
  thumb.rs                # 飞牛图片缩略图（data_dir/thumbs，仅 server）
  settings.rs             # app_data/settings.json（下载目录等）
  tray.rs                 # 系统托盘
  telegram/               # grammers 连接、登录、对话列表、消息库
docker/                   # 飞牛 Compose：Dockerfile + docker-compose.yml
docker/fpk/               # 飞牛离线 .fpk（fnpack，自用手动安装）
```

## 命令

```sh
pnpm tauri:dev            # 桌面开发
pnpm server:dev           # headless HTTP（需 feature server）
pnpm docker:build         # linux/amd64 镜像 tgd:0.1.13（Cargo profile docker，不改桌面 release）
pnpm fpk:build            # 把镜像打进离线 .fpk（dist-fpk/，含 FN Connect 统一网关）
pnpm bindings             # 重生成 src/lib/bindings.ts
pnpm check                # 前端类型检查
pnpm build                # 前端静态构建
pnpm tauri:build:mac      # mac .app + .dmg
cargo test --manifest-path src-tauri/Cargo.toml
```

## 硬规则

1. **前端只通过 `$lib/api` 调 Rust**（桌面转 `$lib/bindings`，浏览器转 `/api`）。不要手写 `invoke("...")`。
2. 新 command 必须同时加 `#[tauri::command]`、`#[specta::specta]`，并挂进 `src-tauri/src/lib.rs` 的 `collect_commands!`。改完跑 `pnpm bindings`。
3. 自定义结构体要 `Serialize` + `specta::Type`；事件再加 `tauri_specta::Event`。
4. 关窗口必须 `hide` + `prevent_close`。退出只走托盘「退出 tgd」或 `quit_app`。
5. 不要提交 `*.session`、`.env` 真凭据。会话在 `app_data_dir()`。下载目录默认同路径下 `downloads`，用户可在设置里更改，路径写入 `settings.json`。监听下载名单是 `watched_chat_ids`；每群类型是 `chat_download_types`（默认全关）；回爬天数全局 `backfill_days`（默认 0 = 只收新，`< 0` = 全量），每群覆盖 `chat_backfill_days`。未加入公开预览是 `guest_watch_*`（开关 + 一个 `@`/`t.me` 公开链接，不 JoinChannel，与已加入监听并存；实时 45s 轮询 `getHistory`）。回爬并行数是 `download_concurrency`（默认 2，范围 1–8）；槽位空出立刻从队列补任务，不必等整批结束。下载页显示按块进度，可取消单文件或暂停全部（`download_paused` 写入 `settings.json`，重启后仍暂停）。未完成文件以 `.part` 按 512KB 对齐续传；暂停/限流/出错保留，单文件取消会删 `.part`。回爬批次若取消、关监听或失败不推进游标，再打开从上次成功点继续（已下完的靠索引跳过）。设置和下载页显示目录实际占用（按会话/类型，含 `.part`）。下载网格按行虚拟滚动；图片/视频/音频点开灯箱，格子里可直接播视频。消息列表是否显示媒体是 `show_media`（默认关）。开机自启是 `autostart`（默认关，启动时同步系统登录项；登录项带 `--hidden`，只进托盘）。SOCKS5 代理是 `proxy_url`（空 = 直连，设置页可改，保存后重连）。会话别名是 `chat_aliases`。会话列表含群组、频道、机器人私聊（`Peer::User` 且 `is_bot`）。文本消息在 `messages.db`（sqlx + SQLite + FTS5），唯一约束 `(chat_id, message_id)`。媒体去重索引是 `media-index.json`（记下过的 `file_id` 即使本地文件已归档/删除也不重下；「清除消息」会丢掉该会话磁盘上已不存在的索引，重爬时文件还在则跳过、不在则重下），游标是 `sync-state.json`。大于 4MB 的媒体走文件所在 DC 的额外 MTProto 连接并行分块（最多 8 条，按并发拆分，同时最多 4 个大文件）。连接池复用；第一条谁先通（media_only 优先）就开始拉块，其余串行补齐以免 exportAuthorization 限流。主连接只在额外连接全失败时兜底。锁等待与单块 30s 超时，死连接当场重建，失败回退主连接串行。下载前必须 `is_watched` + 勾选类型 + `file_id` 去重。关掉某会话监听（或类型全关）会让该会话正在下载的文件下完，立刻丢掉队列里还没开始的任务和回爬剩余，不会删已落盘文件。更新流断开会指数退避重连（2s 起，上限 60s）。退出登录会停 worker 并删除 `telegram.session`，不删消息库和已下载媒体。设置里展示当前登录身份（`get_me` 缓存）。新消息入库或媒体落盘会发 `ChatIngested`，当前打开的会话列表会刷新。本地下载文件和下载目录用系统默认程序打开，且只能打开下载目录内的路径。下载页可按会话下拉筛选。侧栏「搜索」跨会话检索已入库正文。中途新勾媒体类型会记下 `applied_types`，天数不为 0 且本地已有覆盖窗口的记录时按 `message_id` 定点拉（`get_messages_by_id`），不重置历史游标；本地没有可用 id 则退回整段回扫。新勾「文本」仍从头扫。`days == 0` 仍只收新，关了再勾同一类型不重扫。回爬完成后重启或改设置会从最新往回补离线空洞，撞上本地已有最大 `message_id` 即停；live 开 `catch_up`。顶栏：窗口天数不为 0 且已完成显示「回爬已完成」，只有天数是 0 才显示「只收新消息」。
6. grammers 0.10 入口是 `SenderPool::new` / `with_configuration` + `Client::new`，不是旧版 `Client::connect`。走 SOCKS5 时用 `ConnectionParams.proxy_url`（需 feature `proxy`）。session 用 `SqliteSession`。
7. `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 只在 Rust 读仓库根 `.env` 或容器环境变量，不要传到前端，也不要打日志。空字符串（`TELEGRAM_API_ID=`）要当未配置，飞牛向导字段名是 `wizard_telegram_api_id` / `wizard_telegram_api_hash`。FPK 凭据写在 `${TRIM_PKGETC}/telegram.env`，compose 默认下载目录是 `runtime.env` 的 `TGD_DOWNLOAD_DIR`；用户在设置页改过则 `settings.json` 的 `download_dir` 优先。compose `env_file` 指向这两个文件，不要把 `TGD_DOWNLOAD_DIR` 写进 compose `environment`（会盖掉 env_file）。改「访问权限」也会跑 `config_callback`，API 字段为空时必须沿用已有 telegram.env。额外授权路径用 compose override 按原路径挂进容器。`pnpm fpk:build` 必须先重编镜像（Web UI 在镜像里）。 Headless 数据目录是 `TGD_DATA_DIR`。打 `tgd-server` 必须 `--no-default-features --features server`（不编 Tauri/GTK）。桌面默认 feature 是 `desktop`；Tauri CLI 会加 `--no-default-features`，所以 `tauri.conf.json` 的 `build.features` 含 `desktop`。
8. 补 UI 用 `pnpm dlx shadcn-svelte@latest add <component>`，不要从 shadcn-vue / shadcn/ui 抄 React/Vue 代码。
9. 注释、文档、提交说明用中文。代码标识用英文。

## 当前未做

- 搜索结果跳到具体消息
- 开机自启在系统设置里改过后，下次启动仍以 `settings.json` 为准
- 实时新消息中途暂停后不会自动续下（回爬会按 `.part` 续）
- 本地只入库了少量新消息、历史没勾过文本时，新勾媒体类型可能只定点到已入库的 id；可「清除消息」再爬
- 关很久之后 Telegram 可能不再推离线 updates；启动补洞没入库文本时会重走到天数截止
- 未加入公开群很多要成员才能读历史，服务端会拒绝；公开频道一般可以预览

## 打包

- mac 目标：`app` + `dmg`，最低系统 10.15
- Windows 目标已写 `nsis`，初期不作为验证门槛
