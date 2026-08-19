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

## 目录

```
src/                      # SvelteKit
  lib/bindings.ts         # specta 生成，禁止手改
  lib/app-state.svelte.ts # 前端共享状态（连接 / 会话 / 下载）
  lib/components/ui/      # shadcn-svelte
  routes/                 # `/` 会话，`/search` 跨群搜索，`/downloads` 下载，`/settings` 设置
src-tauri/src/
  lib.rs                  # 插件、specta、关窗进托盘
  commands.rs             # 导出给前端的命令 / 事件
  settings.rs             # app_data/settings.json（下载目录等）
  tray.rs                 # 系统托盘
  telegram/               # grammers 连接、登录、对话列表、消息库
```

## 命令

```sh
pnpm tauri:dev            # 桌面开发
pnpm bindings             # 重生成 src/lib/bindings.ts
pnpm check                # 前端类型检查
pnpm build                # 前端静态构建
pnpm tauri:build:mac      # mac .app + .dmg
cargo test --manifest-path src-tauri/Cargo.toml
```

## 硬规则

1. **前端只通过 `$lib/bindings` 调 Rust**，不要手写 `invoke("...")`。
2. 新 command 必须同时加 `#[tauri::command]`、`#[specta::specta]`，并挂进 `src-tauri/src/lib.rs` 的 `collect_commands!`。改完跑 `pnpm bindings`。
3. 自定义结构体要 `Serialize` + `specta::Type`；事件再加 `tauri_specta::Event`。
4. 关窗口必须 `hide` + `prevent_close`。退出只走托盘「退出 tgd」或 `quit_app`。
5. 不要提交 `*.session`、`.env` 真凭据。会话在 `app_data_dir()`。下载目录默认同路径下 `downloads`，用户可在设置里更改，路径写入 `settings.json`。监听下载名单是 `watched_chat_ids`；每群类型是 `chat_download_types`（默认全关）；回爬天数全局 `backfill_days`（默认 0 = 只收新，`< 0` = 全量），每群覆盖 `chat_backfill_days`。回爬并行数是 `download_concurrency`（默认 2，范围 1–8）。下载页显示按块进度，可取消单文件或暂停全部（`download_paused` 写入 `settings.json`，重启后仍暂停）。未完成文件以 `.part` 按 512KB 对齐续传；暂停/限流/出错保留，单文件取消或关掉监听会删 `.part`。回爬批次若取消、关监听或失败不推进游标，再打开从上次成功点继续（已下完的靠索引跳过）。设置和下载页显示目录实际占用（按会话/类型，含 `.part`）。下载网格按行虚拟滚动；图片/视频/音频点开灯箱，格子里可直接播视频。消息列表是否显示媒体是 `show_media`（默认关）。开机自启是 `autostart`（默认关，启动时同步系统登录项；登录项带 `--hidden`，只进托盘）。会话别名是 `chat_aliases`。文本消息在 `messages.db`（sqlx + SQLite + FTS5），唯一约束 `(chat_id, message_id)`。媒体去重索引是 `media-index.json`（记下过的 `file_id` 即使本地文件已归档/删除也不重下），游标是 `sync-state.json`。下载前必须 `is_watched` + 勾选类型 + `file_id` 去重。关掉某会话监听（或类型全关）会立刻取消该会话进行中的下载并丢掉它的回爬队列，不会删已落盘文件。更新流断开会指数退避重连（2s 起，上限 60s）。退出登录会停 worker 并删除 `telegram.session`，不删消息库和已下载媒体。设置里展示当前登录身份（`get_me` 缓存）。新消息入库或媒体落盘会发 `ChatIngested`，当前打开的会话列表会刷新。本地下载文件和下载目录用系统默认程序打开，且只能打开下载目录内的路径。下载页可按会话下拉筛选。侧栏「搜索」跨会话检索已入库正文。中途新勾媒体类型会记下 `applied_types`，天数不为 0 且本地已有覆盖窗口的记录时按 `message_id` 定点拉（`get_messages_by_id`），不重置历史游标；本地没有可用 id 则退回整段回扫。新勾「文本」仍从头扫。`days == 0` 仍只收新，关了再勾同一类型不重扫。回爬完成后重启或改设置会从最新往回补离线空洞，撞上本地已有最大 `message_id` 即停；live 开 `catch_up`。顶栏：窗口天数不为 0 且已完成显示「回爬已完成」，只有天数是 0 才显示「只收新消息」。
6. grammers 0.10 入口是 `SenderPool::new` + `Client::new`，不是旧版 `Client::connect`。session 用 `SqliteSession`。
7. `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 只在 Rust 读仓库根 `.env`，不要传到前端，也不要打日志。
8. 补 UI 用 `pnpm dlx shadcn-svelte@latest add <component>`，不要从 shadcn-vue / shadcn/ui 抄 React/Vue 代码。
9. 注释、文档、提交说明用中文。代码标识用英文。

## 当前未做

- 搜索结果跳到具体消息
- 开机自启在系统设置里改过后，下次启动仍以 `settings.json` 为准
- 实时新消息中途暂停后不会自动续下（回爬会按 `.part` 续）
- 本地只入库了少量新消息、历史没勾过文本时，新勾媒体类型可能只定点到已入库的 id；可「清除消息」再爬
- 关很久之后 Telegram 可能不再推离线 updates；启动补洞没入库文本时会重走到天数截止

## 打包

- mac 目标：`app` + `dmg`，最低系统 10.15
- Windows 目标已写 `nsis`，初期不作为验证门槛
