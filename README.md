# tgd

Telegram 桌面端：长期托盘常驻，静默抓取/监控自己账号的群组、频道、好友消息，并下载图片/视频/文档/音频。

当前可以登录自己的账号，查询已加入的群组 / 频道，把对话加入监听名单并勾选要下的类型（视频 / 音频 / 图片 / 文档 / 文本）。默认不回爬历史（天数为 0），只收打开监听之后的新消息；全局或单群把天数调成大于 0 才会慢慢回爬。已下载的媒体按 Telegram `file_id` 去重，归档或删除本地文件后也不重下。左侧「会话」点开群组即可查看该群已入库记录，支持当前会话关键词搜索；「搜索」可跨群检索已入库正文。入库后当前打开的会话会自动刷新。会话详情可设别名，有别名时列表显示别名。「下载」页看进行中的文件和已落盘媒体，可按会话筛选，网格虚拟滚动；图片 / 视频 / 音频点开灯箱，格子里可直接播视频，也可打开系统默认程序。设置和下载页显示目录占用（按会话/类型）。设置里可打开「消息列表显示媒体」，已下载的图片 / 视频 / 音频会内嵌显示，也可进灯箱。登录身份、开机自启和下载目录也在「设置」，目录可直接打开。开机自启后只进托盘，不弹窗口。暂停下载会记住，重启后仍暂停；未完成的 `.part` 会接着下。

## 栈

- 桌面：Tauri 2（mac 优先，Windows 预留 NSIS）
- 前端：Svelte 5 + SvelteKit（`adapter-static` SPA）+ Vite + Tailwind CSS v4 + shadcn-svelte + `@lucide/svelte` + `@tanstack/svelte-virtual`
- 类型同步：tauri-specta → `src/lib/bindings.ts`
- Telegram：grammers-client 0.10（`SqliteSession` + 手机号登录 + 群组/频道列表）

图标库用 `@lucide/svelte`（Lucide 官方 Svelte 包，对应原先的 lucide-svelte）。

## 环境

- Node 24+（已用 pnpm 11）
- Rust stable（1.77+，本机验证 1.97）
- macOS：Xcode Command Line Tools

```sh
corepack enable
corepack prepare pnpm@latest --activate
pnpm install
```

## 开发

```sh
# 桌面端（推荐）
pnpm tauri:dev

# 只跑前端（看不到托盘，也调不了 Rust 命令）
pnpm dev
```

关窗口会隐藏到托盘，进程不退出。左键托盘图标切换显示，右键菜单可显示 / 隐藏 / 退出。

## 类型同步

改了 `src-tauri` 里带 `#[specta::specta]` 的命令或 `Type` 之后：

```sh
pnpm bindings
```

调试启动 `pnpm tauri:dev` 时也会重写 `src/lib/bindings.ts`。不要手改这个文件。

## 打包

```sh
# macOS：.app + .dmg
pnpm tauri:build:mac

# 也带上 Windows NSIS 配置（需在 Windows 或交叉环境才能真正打出安装包）
pnpm tauri:build
```

产物在 `src-tauri/target/release/bundle/`。

## Telegram 凭据

复制 `.env.example` 为 `.env`，填入 [my.telegram.org](https://my.telegram.org) 的 `TELEGRAM_API_ID` / `TELEGRAM_API_HASH`。不要提交 `.env`。

登录后会话写到系统 app data（macOS: `~/Library/Application Support/com.tgd.app/telegram.session`），不会进仓库。下次启动会复用，不必重复验证。设置里可退出登录，只作废会话，本地消息和已下载文件保留。同步断线会自动重连。

下载目录默认是同路径下的 `downloads`。可在设置里点「更改…」选文件夹，选择会写入 `settings.json`，下次启动沿用。不搬已有文件。设置和下载页都能打开该目录。

群组 / 频道默认不监听。会话详情打开「监听下载」后，id 写入 `settings.json` 的 `watched_chat_ids`。还要勾选类型才会下；类型写在 `chat_download_types`。关掉开关会立刻停该会话未完成的下载，不会删除已下载文件。

回爬天数写在 `backfill_days`（全局，默认 0）和 `chat_backfill_days`（某群覆盖）。0 = 只收新消息，小于 0 = 全量回爬。中途新勾媒体类型会在天数不为 0 时按已入库 `message_id` 定点拉取（省流量）；本地没有覆盖窗口的记录则退回整段回扫。新勾文本仍从头扫。天数为 0 仍只收之后的新消息。已经回爬完成后再启动，会从最新往回补上离线期间的消息，碰到本地已有的最新 id 就停；实时更新会 catch-up。窗口已完成时顶栏显示「回爬已完成」，只有天数是 0 才是「只收新消息」。文本消息存在 app data 的 `messages.db`（SQLite + FTS5），`(chat_id, message_id)` 唯一，回爬不会重复写入。会话页右侧可按当前群翻页查看，并在当前会话内检索；侧栏「搜索」可跨会话检索。媒体按类型分子目录。同一 `file_id` 只下一份，记录在 `media-index.json`；本地文件归档或删除后也不会再下。

语音算音频，圆形视频 / GIF 算视频，贴纸不下。回爬可并行下媒体（设置里改并发，默认 2，范围 1–8）；实时新消息仍串行。翻页和每批下载之间有间隔；遇到 `FLOOD_WAIT` 会停够再继续。「下载」页能看每份文件的进度，可取消某一份或暂停全部。暂停写入 `settings.json`；未完成文件按 512KB 对齐续传，取消单文件会丢掉 `.part`。设置和下载页都能看到目录占用。

## 验证

```sh
pnpm check
pnpm build
cargo test --manifest-path src-tauri/Cargo.toml
```
