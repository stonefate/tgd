# tgd

Telegram 桌面与 NAS 监控抓取工具：支持托盘常驻与后台静默抓取，监听指定群组、频道、私聊消息，并按需自动下载图片、视频、音频与文档。

提供 **飞牛 OS (fnOS) / Docker Headless Web 端** 与 **Tauri 2 桌面客户端** 双形态。

> [!NOTE]
> **平台支持与运行说明**：
> - **飞牛 OS (fnOS) / NAS 端**：**本项目的主力运行环境**，已在飞牛 NAS 实际环境中经过实机稳定运行与抓取测试。
> - **桌面端 (macOS / Windows)**：基于 Tauri 2 开发，功能与 NAS 端共享同一套 Rust 核心业务。因个人精力有限，桌面端需使用者自行编译并测试验证。

---

## ✨ 核心特性

- 🖥️ **双形态支持**：
  - **NAS / Headless 端（主力推荐）**：基于 Axum 的轻量 Web UI 服务，支持 Docker Compose 部署与飞牛 OS 离线 `.fpk` 一键安装，实机长期稳定运行。
  - **桌面端**：基于 Tauri 2，支持 macOS / Windows，最小化到系统托盘静默运行，支持开机自启（需自行测试验证）。
- 📥 **精细化监听与过滤**：
  - **白名单监控**：默认不监听，支持按群组 / 频道 / 机器人单独开启监听。
  - **类型多选**：可按需勾选视频、音频、图片、文档或纯文本入库。
  - **未加入公开频道预览**：无需加入群组，仅凭 `@用户名` 或 `t.me` 链接即可直接监控公开频道/群组（约 45s 周期轮询）。
  - **体积下限过滤**：支持设置 `min_media_mb`（例如跳过小于 10MB 的小视频/动图，文本仍正常入库）。
- ⚡ **分块并发与高速下载**：
  - **多 DC 并行分块**：大于 4MB 的媒体自动建立对应 DC 的多条直连通道并发拉取，提速显著。
  - **断点续传**：未完成文件以 `.part` 形式存储（512KB 对齐），支持暂停、重启续传，单文件取消自动清理。
  - **并发控制**：可配置回爬并发任务数（1–8 个），避免触发 Telegram 限流。
- 🔍 **全文检索与媒体管理**：
  - **消息库与搜索**：内置 SQLite + FTS5 全文检索引擎，支持跨群检索已入库正文与会话内搜索。
  - **媒体去重**：基于 Telegram `file_id` 建立去重索引，本地文件归档或删除后绝不重复下载。
  - **媒体灯箱与播放**：内置图片、音频查看器与视频直接播放，支持虚拟滚动流畅加载海量文件。
- 🛡️ **安全与隔离**：
  - 凭据隔离存储，支持 SOCKS5 本地代理（含账号密码鉴权）。
  - 关窗口默认隐藏至托盘，防止误关中断下载任务。

---

## 🛠️ 技术栈

| 模块 | 技术选型 | 说明 |
| :--- | :--- | :--- |
| **NAS 服务端** | Axum Web 框架 + SSE | 主力环境，Headless 服务端，支持局域网及 FN Connect |
| **桌面框架** | Tauri 2 (`com.tgd.app`) | macOS / Windows NSIS（需自行编译测试） |
| **前端框架** | Svelte 5 + SvelteKit | SPA 模式 (`adapter-static`, `ssr = false`) |
| **样式与组件** | Tailwind CSS v4 + shadcn-svelte | 现代 UI，zinc 色系 |
| **图标与虚拟化**| `@lucide/svelte` + `@tanstack/svelte-virtual` | 高性能流式列表与规范图标 |
| **类型绑定** | `tauri-specta` v2 | Rust 与 TypeScript 命令/事件强类型同步 |
| **Telegram 核心**| `grammers-client` 0.10 + `SqliteSession` | 用户级 MTProto 协议支持 |

---

## 🚀 快速上手

### 环境要求

- **Node.js** 24+（推荐使用 **pnpm 11**）
- **Rust** 1.77+（Stable）
- **macOS**：Xcode Command Line Tools
- **Telegram 开发者凭据**：前往 [my.telegram.org](https://my.telegram.org) 申请获取 `api_id` 与 `api_hash`。

### 1. 安装依赖与配置

```sh
# 启用 corepack 并安装依赖
corepack enable
corepack prepare pnpm@latest --activate
pnpm install

# 配置 Telegram 凭据（凭据已被 gitignore，切勿提交）
cp .env.example .env
# 编辑 .env 填入你的 TELEGRAM_API_ID 和 TELEGRAM_API_HASH
```

### 2. 本地开发

```sh
# 启动桌面开发端（推荐）
pnpm tauri:dev

# 仅启动前端开发服务器（用于 UI 调试）
pnpm dev

# 启动 Headless Web 服务端（用于浏览器端调试）
pnpm server:dev
```

### 3. 应用打包

```sh
# macOS 安装包打包（生成 .app 与 .dmg）
pnpm tauri:build:mac

# 通用打包
pnpm tauri:build
```
> 打包产物位于 `src-tauri/target/release/bundle/`。

---

## 🦹 飞牛 OS (fnOS) 与 Docker 部署

桌面端业务与 NAS Web 端完全统一。Web UI 默认运行在 `8787` 端口（访问路径 `/app/tgd`）。

### 方案 A：飞牛 Docker Compose 部署

1. **构建并导出镜像**（默认 `linux/amd64`，适配 x86_64 NAS）：
   ```sh
   pnpm docker:build
   docker save tgd:0.1.15 | gzip > tgd-0.1.15.tar.gz
   ```
2. **导入 NAS**：
   将 `tgd-0.1.15.tar.gz` 上传至 NAS 并解压载入：
   ```sh
   gzip -dc tgd-0.1.15.tar.gz | docker load
   ```
3. **启动 Compose**：
   在飞牛 Docker 的 Compose 管理中新建项目，使用仓库中的 `docker/docker-compose.yml`，并在同级目录配置 `.env` 填入 `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` 即可。

### 方案 B：飞牛离线安装包（.fpk）

1. **一键制作离线安装包**：
   ```sh
   pnpm docker:build
   pnpm fpk:build
   ```
2. **安装**：
   产物位于 `dist-fpk/tgd.fpk`。打开 **飞牛 OS ➜ 应用中心 ➜ 设置 ➜ 手动安装**，选择该 `.fpk` 文件。
3. **向导配置**：
   在安装向导中输入 `api_id` 与 `api_hash`。安装后桌面生成图标，直接点击即可通过局域网或 FN Connect 远程访问。

---

## ⚙️ 核心机制与数据存储

### 1. 回爬与下载策略
- **回爬天数**：默认为 `0`（只收开启监听后的实时新消息）。设置为 `> 0` 时会按指定天数向历史回扫，设置为 `< 0` 则全量回爬。
- **离线补洞**：重启或网络恢复后，会自动从最新消息往回补齐断线期间的消息，命中本地最新 ID 后自动停止。
- **动态定点拉取**：历史回爬完成后若新增勾选媒体类型，会优先根据本地消息 ID 定点拉取媒体，节约流量与时间。

### 2. 数据目录与文件存储
- **桌面端存储路径**：
  - macOS: `~/Library/Application Support/com.tgd.app/`
  - 包含 `telegram.session`（登录态）、`messages.db`（SQLite 消息库）、`settings.json`（用户配置）、`media-index.json`（去重索引）。
- **默认下载目录**：
  - 位于应用数据目录下的 `downloads/`，可在「设置」中随时更改，下载目录内按媒体类型及会话分类存储。

---

## 🧪 验证与自检

修改代码后可执行以下命令完成自检：

```sh
# 重生成 specta 类型绑定
pnpm bindings

# 前端类型检查
pnpm check

# 前端静态构建测试
pnpm build

# Rust 单元测试
cargo test --manifest-path src-tauri/Cargo.toml
```

---

## 📄 开源协议

本项目采用 [GNU General Public License v3.0 (GPL-3.0)](./LICENSE) 协议开源。
