//! 微信 bot 文本指令：绑定的机主发 `/` 开头的消息触发操作，bot 回执结果。

use crate::runtime::AppCtx;
use crate::service;
use crate::settings::ChatDownloadTypes;
use crate::telegram::{ChatItem, DownloadPhase};

/// 回复文本长度安全线，超出截断（微信文本消息上限不明，取保守值）
const REPLY_MAX: usize = 2800;
/// 列表最多显示的行数
const LIST_MAX: usize = 60;
/// /运行日志 返回的条数
const LOG_LINES: usize = 15;

/// 处理一条入站文本：非 `/` 开头返回 None（不回复），否则返回回执文本
pub async fn handle_command(ctx: &AppCtx, raw: &str) -> Option<String> {
    let text = normalize(raw);
    if !text.starts_with('/') {
        return None;
    }
    Some(clip(dispatch(ctx, &text).await))
}

/// 全角斜杠归一成半角，其余原样
fn normalize(raw: &str) -> String {
    let text = raw.trim();
    match text.strip_prefix('／') {
        Some(rest) => format!("/{rest}"),
        None => text.to_string(),
    }
}

async fn dispatch(ctx: &AppCtx, text: &str) -> String {
    let mut parts = text.split_whitespace();
    let cmd = parts.next().unwrap_or("/").to_string();
    let args: Vec<&str> = parts.collect();
    match cmd.as_str() {
        "/" | "/帮助" => help_text(),
        "/暂停下载" => toggle_pause(ctx, true).await,
        "/继续下载" => toggle_pause(ctx, false).await,
        "/下载状态" => status_text(ctx).await,
        "/监听列表" => list_text(ctx, true).await,
        "/未监听列表" => list_text(ctx, false).await,
        "/监听" => watch(ctx, &args).await,
        "/取消监听" => unwatch(ctx, &args).await,
        "/检查文件" => check_files(ctx, &args).await,
        "/别名" => set_alias(ctx, &args).await,
        "/运行日志" => logs_text(),
        other => format!("未知指令 {other}，回复 / 查看全部指令。\n\n{}", help_text()),
    }
}

fn help_text() -> String {
    [
        "微信指令：",
        "/下载状态 — 回爬与下载概况",
        "/暂停下载、/继续下载",
        "/监听列表 — 已监听群组（序号用于 /取消监听、/检查文件、/别名）",
        "/未监听列表 — 未监听群组（序号用于 /监听）",
        "/监听 序列号 [回爬天数] 媒体类型 — 如 /监听 3 7 视频|音频；类型：视频|音频|图片|文档|文本，可多选；天数省略用全局，0=只收新，负数=全量",
        "/取消监听 序列号",
        "/检查文件 序列号 — 补齐该群缺失的媒体文件",
        "/别名 序列号 名称 — 名称填 - 清除",
        "/运行日志 — 最近警告/错误",
    ]
    .join("\n")
}

async fn toggle_pause(ctx: &AppCtx, paused: bool) -> String {
    match service::set_download_paused(ctx, paused) {
        Ok(_) if paused => "已暂停下载，回复 /继续下载 恢复。".into(),
        Ok(_) => "已恢复下载。".into(),
        Err(err) => format!("操作失败：{err}"),
    }
}

async fn status_text(ctx: &AppCtx) -> String {
    let progress = service::get_download_status(ctx);
    let phase = match progress.phase {
        DownloadPhase::Idle => "空闲",
        DownloadPhase::Live => "实时监听",
        DownloadPhase::Backfill => "回爬中",
        DownloadPhase::Wait => "等待",
        DownloadPhase::FloodWait => "限流等待",
        DownloadPhase::Reconnect => "断线重连",
    };
    let mut lines = vec![format!("状态：{phase}")];
    if let Some(title) = progress
        .chat_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        lines.push(format!("当前会话：{title}"));
    }
    lines.push(format!(
        "本次批次：处理 {}，下载 {}，跳过 {}",
        progress.processed, progress.downloaded, progress.skipped
    ));
    lines.push(format!(
        "下载队列：进行中 {}，等待 {}",
        progress.active.len(),
        progress.queued.len()
    ));
    if let Some(secs) = progress.flood_wait_secs {
        lines.push(format!("限流：等待 {secs} 秒后重试"));
    }
    if progress.paused {
        lines.push("下载：已暂停（回复 /继续下载 恢复）".into());
    }
    if let Some(detail) = progress
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    {
        lines.push(format!("详情：{detail}"));
    }
    lines.join("\n")
}

struct ChatLists {
    watched: Vec<ChatItem>,
    unwatched: Vec<ChatItem>,
}

/// 未加入的公开预览群（guest）不参与指令；两组各自按显示名排序，序号稳定
async fn load_lists(ctx: &AppCtx) -> Result<ChatLists, String> {
    let chats = service::list_chats(ctx, false)
        .await
        .map_err(|err| err.to_string())?;
    let mut watched: Vec<ChatItem> = chats
        .iter()
        .filter(|chat| chat.watched && !chat.guest)
        .cloned()
        .collect();
    let mut unwatched: Vec<ChatItem> = chats
        .iter()
        .filter(|chat| !chat.watched && !chat.guest)
        .cloned()
        .collect();
    watched.sort_by(|a, b| display_title(a).cmp(&display_title(b)));
    unwatched.sort_by(|a, b| display_title(a).cmp(&display_title(b)));
    Ok(ChatLists { watched, unwatched })
}

fn display_title(chat: &ChatItem) -> &str {
    let alias = chat
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|alias| !alias.is_empty());
    alias.unwrap_or(&chat.title)
}

async fn list_text(ctx: &AppCtx, watched: bool) -> String {
    let lists = match load_lists(ctx).await {
        Ok(lists) => lists,
        Err(msg) => return format!("获取会话列表失败：{msg}"),
    };
    let items = if watched {
        &lists.watched
    } else {
        &lists.unwatched
    };
    if items.is_empty() {
        return if watched {
            "暂无已监听的群组/频道。回复 /未监听列表 查看可监听的群。".into()
        } else {
            "所有群组/频道都已监听。".into()
        };
    }
    let mut lines: Vec<String> = items
        .iter()
        .enumerate()
        .take(LIST_MAX)
        .map(|(i, chat)| format!("{}. {}", i + 1, display_title(chat)))
        .collect();
    if items.len() > LIST_MAX {
        lines.push(format!("…其余 {} 个已省略", items.len() - LIST_MAX));
    }
    lines.join("\n")
}

async fn watch(ctx: &AppCtx, args: &[&str]) -> String {
    let usage = "用法：/监听 序列号 [回爬天数] 媒体类型，如 /监听 3 7 视频|音频；类型：视频|音频|图片|文档|文本，可多选用 | 分隔；天数省略用全局，0=只收新，负数=全量";
    let Some(index) = args.first().and_then(|raw| parse_index(raw)) else {
        return usage.to_string();
    };
    let (days, type_words) = split_days(&args[1..]);
    if type_words.is_empty() {
        return usage.to_string();
    }
    let types = match build_types(&type_words) {
        Ok(types) => types,
        Err(bad) => {
            return format!("不认识的媒体类型「{bad}」。可选：视频|音频|图片|文档|文本。");
        }
    };
    let lists = match load_lists(ctx).await {
        Ok(lists) => lists,
        Err(msg) => return format!("获取会话列表失败：{msg}"),
    };
    let Some(chat) = lists.unwatched.get(index - 1) else {
        return format!("未监听列表没有序号 {index}，回复 /未监听列表 查看。");
    };
    let chat_id = chat.id.clone();
    let title = display_title(chat).to_string();
    // 先落类型与天数再开监听，回爬入队时就能拿到正确配置
    if let Err(err) = service::set_chat_download_types(ctx, chat_id.clone(), types) {
        return format!("操作失败：{err}");
    }
    if let Some(days) = days {
        if let Err(err) = service::set_chat_backfill_days(ctx, chat_id.clone(), Some(days)) {
            return format!("操作失败：{err}");
        }
    }
    match service::set_chat_watched(ctx, chat_id, true).await {
        Ok(_) => format!(
            "已监听「{title}」，{}。回复 /监听列表 查看监听列表。",
            describe(days, &types)
        ),
        Err(err) => format!("操作失败：{err}"),
    }
}

async fn unwatch(ctx: &AppCtx, args: &[&str]) -> String {
    let Some(index) = args.first().and_then(|raw| parse_index(raw)) else {
        return "用法：/取消监听 序列号（序号来自 /监听列表）".to_string();
    };
    let lists = match load_lists(ctx).await {
        Ok(lists) => lists,
        Err(msg) => return format!("获取会话列表失败：{msg}"),
    };
    let Some(chat) = lists.watched.get(index - 1) else {
        return format!("监听列表没有序号 {index}，回复 /监听列表 查看。");
    };
    let chat_id = chat.id.clone();
    let title = display_title(chat).to_string();
    match service::set_chat_watched(ctx, chat_id, false).await {
        Ok(_) => format!(
            "已取消监听「{title}」。正在下载的文件会下完，未开始的任务立即丢弃，已落盘文件保留。"
        ),
        Err(err) => format!("操作失败：{err}"),
    }
}

async fn check_files(ctx: &AppCtx, args: &[&str]) -> String {
    let Some(index) = args.first().and_then(|raw| parse_index(raw)) else {
        return "用法：/检查文件 序列号（序号来自 /监听列表）".to_string();
    };
    let lists = match load_lists(ctx).await {
        Ok(lists) => lists,
        Err(msg) => return format!("获取会话列表失败：{msg}"),
    };
    let Some(chat) = lists.watched.get(index - 1) else {
        return format!("监听列表没有序号 {index}，回复 /监听列表 查看。");
    };
    let title = display_title(chat).to_string();
    match service::check_chat_media(ctx, chat.id.clone()).await {
        Ok(true) => format!("已开始检查「{title}」的缺失文件，发现缺漏会自动补下载。"),
        Ok(false) => {
            format!("「{title}」没有产生检查任务：需要已监听、回爬天数非 0 且勾选了媒体类型。")
        }
        Err(err) => format!("操作失败：{err}"),
    }
}

async fn set_alias(ctx: &AppCtx, args: &[&str]) -> String {
    let Some(index) = args.first().and_then(|raw| parse_index(raw)) else {
        return "用法：/别名 序列号 名称（名称填 - 清除，序号来自 /监听列表）".to_string();
    };
    let name = args[1..].join(" ");
    let name = name.trim();
    let alias = if name.is_empty() || name == "-" {
        None
    } else {
        Some(name.to_string())
    };
    let lists = match load_lists(ctx).await {
        Ok(lists) => lists,
        Err(msg) => return format!("获取会话列表失败：{msg}"),
    };
    let Some(chat) = lists.watched.get(index - 1) else {
        return format!("监听列表没有序号 {index}，回复 /监听列表 查看。");
    };
    let title = display_title(chat).to_string();
    match service::set_chat_alias(ctx, chat.id.clone(), alias) {
        Ok(Some(new)) => format!("已把「{title}」的别名设为「{new}」。"),
        Ok(None) => format!("已清除「{title}」的别名。"),
        Err(err) => format!("操作失败：{err}"),
    }
}

fn logs_text() -> String {
    let logs = service::get_recent_logs();
    if logs.is_empty() {
        return "暂无警告/错误日志。".into();
    }
    logs.iter()
        .take(LOG_LINES)
        .map(|entry| format!("{} [{}] {}", entry.time, entry.level, entry.message))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_index(raw: &str) -> Option<usize> {
    let index: usize = raw.trim().parse().ok()?;
    (index >= 1).then_some(index)
}

/// 参数里第一个整数是回爬天数，剩下的是类型词
fn split_days(args: &[&str]) -> (Option<i32>, Vec<String>) {
    if let Some((first, rest)) = args.split_first() {
        if let Ok(days) = first.trim().parse::<i32>() {
            return (Some(days), rest.iter().map(|s| s.to_string()).collect());
        }
    }
    (None, args.iter().map(|s| s.to_string()).collect())
}

/// 类型词用 | 、 ， （含全角）与空白分隔，中英文都认
fn build_types(words: &[String]) -> Result<ChatDownloadTypes, String> {
    let joined = words.join(" ");
    let mut types = ChatDownloadTypes::default();
    for word in joined
        .split(['|', '｜', '、', '，', ' '])
        .map(str::trim)
        .filter(|word| !word.is_empty())
    {
        match word {
            "视频" | "video" => types.video = true,
            "音频" | "audio" => types.audio = true,
            "图片" | "photo" => types.photo = true,
            "文档" | "document" => types.document = true,
            "文本" | "text" => types.text = true,
            other => return Err(other.to_string()),
        }
    }
    Ok(types)
}

fn describe(days: Option<i32>, types: &ChatDownloadTypes) -> String {
    let mut names = Vec::new();
    if types.photo {
        names.push("图片");
    }
    if types.video {
        names.push("视频");
    }
    if types.audio {
        names.push("音频");
    }
    if types.document {
        names.push("文档");
    }
    if types.text {
        names.push("文本");
    }
    let days_text = match days {
        Some(0) => "只收新消息".to_string(),
        Some(n) if n < 0 => "全量回爬".to_string(),
        Some(n) => format!("回爬 {n} 天"),
        None => "回爬天数沿用全局".to_string(),
    };
    format!("{days_text}，类型：{}", names.join("、"))
}

fn clip(text: String) -> String {
    if text.chars().count() <= REPLY_MAX {
        return text;
    }
    let mut out: String = text.chars().take(REPLY_MAX).collect();
    out.push_str("\n…（内容过长已截断）");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::ChatKind;

    fn chat(alias: Option<&str>, title: &str) -> ChatItem {
        ChatItem {
            id: "chat-1".into(),
            kind: ChatKind::Group,
            title: title.into(),
            username: None,
            watched: false,
            types: ChatDownloadTypes::default(),
            backfill_days: 0,
            backfill_days_override: None,
            alias: alias.map(str::to_string),
            comment_of_id: None,
            comment_of_title: None,
            discussion_id: None,
            discussion_joined: None,
            guest: false,
        }
    }

    #[test]
    fn parse_index_bounds() {
        assert_eq!(parse_index("3"), Some(3));
        assert_eq!(parse_index(" 2 "), Some(2));
        assert_eq!(parse_index("0"), None);
        assert_eq!(parse_index("-1"), None);
        assert_eq!(parse_index("x"), None);
    }

    #[test]
    fn split_days_optional() {
        let (days, rest) = split_days(&["7", "视频|音频"]);
        assert_eq!(days, Some(7));
        assert_eq!(rest, vec!["视频|音频".to_string()]);

        let (days, rest) = split_days(&["-1", "视频"]);
        assert_eq!(days, Some(-1));
        assert_eq!(rest, vec!["视频".to_string()]);

        let (days, rest) = split_days(&["视频"]);
        assert_eq!(days, None);
        assert_eq!(rest, vec!["视频".to_string()]);
    }

    #[test]
    fn build_types_mixed_separators() {
        let types = build_types(&["视频|音频".into(), "图片，text".into()]).unwrap();
        assert!(types.video && types.audio && types.photo && types.text);
        assert!(!types.document);

        let err = build_types(&["表情".into()]).unwrap_err();
        assert_eq!(err, "表情");
    }

    #[test]
    fn display_title_prefers_alias() {
        assert_eq!(display_title(&chat(Some("阿群 "), "原始名")), "阿群");
        assert_eq!(display_title(&chat(None, "原始名")), "原始名");
    }

    #[test]
    fn clip_truncates_long_text() {
        let short = clip("短文本".into());
        assert_eq!(short, "短文本");
        let long: String = "啊".chars().cycle().take(REPLY_MAX + 10).collect();
        let clipped = clip(long);
        assert!(clipped.chars().count() < REPLY_MAX + 20);
        assert!(clipped.ends_with("…（内容过长已截断）"));
    }

    #[test]
    fn normalize_fullwidth_slash() {
        assert_eq!(normalize("／帮助"), "/帮助");
        assert_eq!(normalize("  /暂停下载 "), "/暂停下载");
        assert_eq!(normalize("你好"), "你好");
    }
}
