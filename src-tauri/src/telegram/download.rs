use std::collections::HashMap;
use std::path::{Path, PathBuf};

use grammers_client::media::{Document, Media};
use grammers_client::tl::enums::{Document as TlDocument, DocumentAttribute};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::AppError;

/// 用户可勾选的落盘类型。语音算音频；贴纸不下。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Photo,
    Video,
    Document,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Document => "document",
            Self::Audio => "audio",
        }
    }

    pub fn folder(self) -> &'static str {
        self.as_str()
    }
}

/// 从 Document 属性归类时用的可测中间态。
#[derive(Debug, Clone, Default)]
pub struct DocumentHints {
    pub sticker: bool,
    pub video: bool,
    pub animated: bool,
    pub audio: bool,
    pub mime: Option<String>,
}

/// 把 Telegram 媒体映射到我们的四类。贴纸 / 非文件媒体返回 `None`。
pub fn classify_media(media: &Media) -> Option<MediaKind> {
    match media {
        Media::Photo(_) => Some(MediaKind::Photo),
        Media::Sticker(_) => None,
        Media::Document(document) => classify_document(document),
        _ => None,
    }
}

pub fn classify_document(document: &Document) -> Option<MediaKind> {
    classify_document_hints(&document_hints(document))
}

pub fn classify_document_hints(hints: &DocumentHints) -> Option<MediaKind> {
    if hints.sticker {
        return None;
    }
    if hints.video || hints.animated {
        return Some(MediaKind::Video);
    }
    if hints.audio {
        return Some(MediaKind::Audio);
    }
    if let Some(mime) = hints.mime.as_deref() {
        if mime == "image/gif" {
            return Some(MediaKind::Video);
        }
        if mime.starts_with("image/") {
            return Some(MediaKind::Photo);
        }
    }
    Some(MediaKind::Document)
}

fn document_hints(document: &Document) -> DocumentHints {
    let mut hints = DocumentHints::default();
    let Some(TlDocument::Document(raw)) = document.raw.document.as_ref() else {
        return hints;
    };
    hints.mime = Some(raw.mime_type.clone());
    for attr in &raw.attributes {
        match attr {
            DocumentAttribute::Sticker(_) => hints.sticker = true,
            DocumentAttribute::Video(_) => hints.video = true,
            DocumentAttribute::Audio(_) => hints.audio = true,
            DocumentAttribute::Animated => hints.animated = true,
            _ => {}
        }
    }
    hints
}

pub fn media_file_id(media: &Media) -> Option<String> {
    match media {
        Media::Photo(photo) => Some(photo.id().to_string()),
        Media::Document(document) => Some(document.id().to_string()),
        Media::Sticker(sticker) => Some(sticker.document.id().to_string()),
        _ => None,
    }
}

pub fn media_original_name(media: &Media) -> Option<String> {
    match media {
        Media::Document(document) => document
            .name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        Media::Sticker(sticker) => sticker
            .document
            .name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

pub fn media_mime(media: &Media) -> Option<&str> {
    match media {
        Media::Document(document) => document.mime_type(),
        Media::Sticker(sticker) => sticker.document.mime_type(),
        Media::Photo(_) => Some("image/jpeg"),
        _ => None,
    }
}

pub fn extension_for(kind: MediaKind, mime: Option<&str>, original: Option<&str>) -> String {
    if let Some(name) = original {
        if let Some(ext) = Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::trim)
            .filter(|ext| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return ext.to_ascii_lowercase();
        }
    }

    match mime.unwrap_or("") {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/aac" | "audio/mp4" => "m4a",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        _ => match kind {
            MediaKind::Photo => "jpg",
            MediaKind::Video => "mp4",
            MediaKind::Audio => "ogg",
            MediaKind::Document => "bin",
        },
    }
    .to_string()
}

pub fn sanitize_component(name: &str, max_chars: usize) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    let mut out: String = trimmed.chars().take(max_chars.max(1)).collect();
    if out.is_empty() {
        out.push_str("chat");
    }
    out
}

pub fn chat_dir(download_dir: &Path, title: &str, chat_id: &str) -> PathBuf {
    let slug = sanitize_component(title, 40);
    download_dir.join(format!("{slug}_{chat_id}"))
}

pub fn media_path(
    download_dir: &Path,
    title: &str,
    chat_id: &str,
    kind: MediaKind,
    file_id: &str,
    original: Option<&str>,
    mime: Option<&str>,
) -> PathBuf {
    let ext = extension_for(kind, mime, original);
    let stem = match original {
        Some(name) => {
            let file_name = Path::new(name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(name);
            let safe = sanitize_component(file_name, 48);
            format!("{file_id}_{safe}")
        }
        None => file_id.to_string(),
    };
    chat_dir(download_dir, title, chat_id)
        .join(kind.folder())
        .join(format!("{stem}.{ext}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaIndexEntry {
    pub path: PathBuf,
    pub kind: MediaKind,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub chat_title: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub downloaded_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredMedia {
    pub chat_id: Option<String>,
    pub chat_title: Option<String>,
    pub file_name: Option<String>,
}

/// 从 `{标题}_{chatId}/{kind}/{文件名}` 猜会话和文件名，兼容旧索引。
pub fn infer_from_path(path: &Path) -> InferredMedia {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let folder = path
        .parent()
        .and_then(|kind_dir| kind_dir.parent())
        .and_then(|chat_dir| chat_dir.file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let Some(folder) = folder else {
        return InferredMedia {
            chat_id: None,
            chat_title: None,
            file_name,
        };
    };
    match folder.rsplit_once('_') {
        Some((slug, id)) if !id.is_empty() => InferredMedia {
            chat_id: Some(id.to_string()),
            chat_title: {
                let title = slug.replace('_', " ");
                let title = title.trim();
                if title.is_empty() {
                    None
                } else {
                    Some(title.to_string())
                }
            },
            file_name,
        },
        _ => InferredMedia {
            chat_id: None,
            chat_title: Some(folder.to_string()),
            file_name,
        },
    }
}

pub fn file_mtime_unix(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|dur| i64::try_from(dur.as_secs()).ok())
}

/// grammers `iter_download` 默认块大小，续传必须按这个对齐。
pub const DOWNLOAD_CHUNK: u64 = 512 * 1024;

pub fn part_path(dest: &Path) -> PathBuf {
    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(".part");
    PathBuf::from(tmp)
}

pub fn aligned_part_len(len: u64) -> u64 {
    len - (len % DOWNLOAD_CHUNK)
}

pub fn skip_chunks(aligned: u64) -> i32 {
    i32::try_from(aligned / DOWNLOAD_CHUNK).unwrap_or(i32::MAX)
}

fn kind_from_folder(name: &str) -> Option<MediaKind> {
    match name {
        "photo" => Some(MediaKind::Photo),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        "document" => Some(MediaKind::Document),
        _ => None,
    }
}

fn path_without_part(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match name.strip_suffix(".part") {
        Some(stem) if !stem.is_empty() => path.with_file_name(stem),
        _ => path.to_path_buf(),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UsageKindBytes {
    pub photo: u64,
    pub video: u64,
    pub audio: u64,
    pub document: u64,
    pub other: u64,
}

impl UsageKindBytes {
    pub fn add(&mut self, kind: Option<MediaKind>, n: u64) {
        match kind {
            Some(MediaKind::Photo) => self.photo += n,
            Some(MediaKind::Video) => self.video += n,
            Some(MediaKind::Audio) => self.audio += n,
            Some(MediaKind::Document) => self.document += n,
            None => self.other += n,
        }
    }

    pub fn total(&self) -> u64 {
        self.photo + self.video + self.audio + self.document + self.other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageChat {
    pub chat_id: Option<String>,
    pub chat_title: Option<String>,
    pub kinds: UsageKindBytes,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DirUsage {
    pub kinds: UsageKindBytes,
    pub parts: u64,
    pub chats: Vec<UsageChat>,
}

impl DirUsage {
    pub fn total(&self) -> u64 {
        self.kinds.total()
    }
}

/// 扫下载目录实际占用，含 `.part`。不跟索引，归档走了的不算。
pub fn scan_download_dir(root: &Path) -> DirUsage {
    let mut kinds = UsageKindBytes::default();
    let mut parts = 0_u64;
    let mut chats: HashMap<String, UsageChat> = HashMap::new();

    visit_files(root, &mut |path, size| {
        let is_part = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".part"));
        if is_part {
            parts += size;
        }
        let logical = path_without_part(path);
        let kind = logical
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .and_then(kind_from_folder);
        kinds.add(kind, size);

        let inferred = infer_from_path(&logical);
        let key = inferred
            .chat_id
            .clone()
            .unwrap_or_else(|| "__unknown__".into());
        let entry = chats.entry(key).or_insert_with(|| UsageChat {
            chat_id: inferred.chat_id.clone(),
            chat_title: inferred.chat_title.clone(),
            kinds: UsageKindBytes::default(),
        });
        if entry.chat_title.is_none() {
            entry.chat_title = inferred.chat_title;
        }
        entry.kinds.add(kind, size);
    });

    let mut chats: Vec<UsageChat> = chats.into_values().collect();
    chats.sort_by(|left, right| {
        right.kinds.total().cmp(&left.kinds.total()).then_with(|| {
            left.chat_id
                .as_deref()
                .unwrap_or("")
                .cmp(right.chat_id.as_deref().unwrap_or(""))
        })
    });
    DirUsage {
        kinds,
        parts,
        chats,
    }
}

fn visit_files(dir: &Path, visit: &mut impl FnMut(&Path, u64)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            visit_files(&path, visit);
        } else if meta.is_file() {
            visit(&path, meta.len());
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MediaIndex {
    #[serde(default)]
    pub files: HashMap<String, MediaIndexEntry>,
}

impl MediaIndex {
    pub fn file_path(root: &Path) -> PathBuf {
        root.join("media-index.json")
    }

    pub fn load(root: &Path) -> Self {
        let path = Self::file_path(root);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(index) => index,
            Err(err) => {
                log::warn!("failed to parse media-index.json: {err}");
                Self::default()
            }
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), AppError> {
        std::fs::create_dir_all(root).map_err(|err| AppError::Io(err.to_string()))?;
        let raw =
            serde_json::to_string_pretty(self).map_err(|err| AppError::Io(err.to_string()))?;
        std::fs::write(Self::file_path(root), raw).map_err(|err| AppError::Io(err.to_string()))
    }

    /// 索引里有过这条就视为已处理，文件被归档或删掉也不重下。
    pub fn contains(&self, file_id: &str) -> bool {
        self.files.contains_key(file_id)
    }

    /// 索引里有这条且磁盘文件还在，才返回绝对路径（列表/预览用）。
    pub fn existing_path(&self, file_id: &str) -> Option<&Path> {
        self.files.get(file_id).and_then(|entry| {
            if entry.path.exists() {
                Some(entry.path.as_path())
            } else {
                None
            }
        })
    }

    pub fn remember(
        &mut self,
        file_id: String,
        path: PathBuf,
        kind: MediaKind,
        size: Option<u64>,
        chat_id: Option<String>,
        chat_title: Option<String>,
    ) {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let downloaded_at = Some(chrono::Utc::now().timestamp());
        self.files.insert(
            file_id,
            MediaIndexEntry {
                path,
                kind,
                size,
                chat_id,
                chat_title,
                file_name,
                downloaded_at,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_hints() {
        assert_eq!(
            classify_document_hints(&DocumentHints {
                video: true,
                ..DocumentHints::default()
            }),
            Some(MediaKind::Video)
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                animated: true,
                ..DocumentHints::default()
            }),
            Some(MediaKind::Video)
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                audio: true,
                ..DocumentHints::default()
            }),
            Some(MediaKind::Audio)
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                sticker: true,
                video: true,
                ..DocumentHints::default()
            }),
            None
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                mime: Some("image/png".into()),
                ..DocumentHints::default()
            }),
            Some(MediaKind::Photo)
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                mime: Some("image/gif".into()),
                ..DocumentHints::default()
            }),
            Some(MediaKind::Video)
        );
        assert_eq!(
            classify_document_hints(&DocumentHints {
                mime: Some("application/pdf".into()),
                ..DocumentHints::default()
            }),
            Some(MediaKind::Document)
        );
    }

    #[test]
    fn sanitize_and_paths() {
        assert_eq!(sanitize_component("a/b:c", 10), "a_b_c");
        assert_eq!(sanitize_component("...", 10), "chat");
        let dir = PathBuf::from("/tmp/dl");
        let path = media_path(
            &dir,
            "Hello/World",
            "-1001",
            MediaKind::Video,
            "99",
            Some("clip.MP4"),
            Some("video/mp4"),
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/dl/Hello_World_-1001/video/99_clip.mp4")
        );
        assert_eq!(
            infer_from_path(&path),
            InferredMedia {
                chat_id: Some("-1001".into()),
                chat_title: Some("Hello World".into()),
                file_name: Some("99_clip.mp4".into()),
            }
        );
    }

    #[test]
    fn media_index_roundtrip_and_dedup() {
        let root = std::env::temp_dir().join(format!(
            "tgd-media-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("exists.bin");
        std::fs::write(&file, b"ok").unwrap();

        let mut index = MediaIndex::default();
        assert!(!index.contains("1"));
        index.remember(
            "1".into(),
            file.clone(),
            MediaKind::Document,
            Some(2),
            Some("-1001".into()),
            Some("Hello World".into()),
        );
        index.save(&root).unwrap();

        let loaded = MediaIndex::load(&root);
        assert!(loaded.contains("1"));
        assert_eq!(loaded.existing_path("1"), Some(file.as_path()));
        let entry = loaded.files.get("1").unwrap();
        assert_eq!(entry.chat_id.as_deref(), Some("-1001"));
        assert_eq!(entry.chat_title.as_deref(), Some("Hello World"));
        assert_eq!(entry.file_name.as_deref(), Some("exists.bin"));
        assert!(entry.downloaded_at.is_some());
        assert!(!loaded.contains("2"));
        assert!(loaded.existing_path("2").is_none());

        let missing = root.join("gone.bin");
        let mut stale = MediaIndex::default();
        stale.remember("gone".into(), missing, MediaKind::Photo, None, None, None);
        assert!(stale.contains("gone"));
        assert!(stale.existing_path("gone").is_none());

        std::fs::remove_file(&file).unwrap();
        let after_delete = MediaIndex::load(&root);
        assert!(after_delete.contains("1"));
        assert!(after_delete.existing_path("1").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cutoff_helper() {
        let now = chrono::Utc::now();
        assert!(is_before_cutoff(now - chrono::Duration::days(31), 30));
        assert!(!is_before_cutoff(now - chrono::Duration::days(1), 30));
        assert!(is_before_cutoff(now, 0));
        assert!(!is_before_cutoff(now - chrono::Duration::days(4000), -1));
    }

    #[test]
    fn part_align_and_path() {
        let dest = PathBuf::from("/tmp/dl/clip.mp4");
        assert_eq!(part_path(&dest), PathBuf::from("/tmp/dl/clip.mp4.part"));
        assert_eq!(aligned_part_len(0), 0);
        assert_eq!(aligned_part_len(DOWNLOAD_CHUNK - 1), 0);
        assert_eq!(aligned_part_len(DOWNLOAD_CHUNK), DOWNLOAD_CHUNK);
        assert_eq!(aligned_part_len(DOWNLOAD_CHUNK + 100), DOWNLOAD_CHUNK);
        assert_eq!(skip_chunks(0), 0);
        assert_eq!(skip_chunks(DOWNLOAD_CHUNK * 3), 3);
    }

    #[test]
    fn scan_download_dir_by_chat_and_kind() {
        let root = std::env::temp_dir().join(format!(
            "tgd-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let video = root.join("Hello_World_-1001/video");
        let photo = root.join("Hello_World_-1001/photo");
        let other_chat = root.join("Other_-2002/audio");
        std::fs::create_dir_all(&video).unwrap();
        std::fs::create_dir_all(&photo).unwrap();
        std::fs::create_dir_all(&other_chat).unwrap();
        std::fs::write(video.join("a.mp4"), vec![0u8; 40]).unwrap();
        std::fs::write(video.join("a.mp4.part"), vec![0u8; 10]).unwrap();
        std::fs::write(photo.join("b.jpg"), vec![0u8; 7]).unwrap();
        std::fs::write(other_chat.join("c.ogg"), vec![0u8; 5]).unwrap();
        std::fs::write(root.join("orphan.bin"), vec![0u8; 3]).unwrap();

        let usage = scan_download_dir(&root);
        assert_eq!(usage.total(), 65);
        assert_eq!(usage.parts, 10);
        assert_eq!(usage.kinds.video, 50);
        assert_eq!(usage.kinds.photo, 7);
        assert_eq!(usage.kinds.audio, 5);
        assert_eq!(usage.kinds.other, 3);
        assert_eq!(usage.chats.len(), 3);
        assert_eq!(usage.chats[0].chat_id.as_deref(), Some("-1001"));
        assert_eq!(usage.chats[0].kinds.total(), 57);
        assert_eq!(usage.chats[1].chat_id.as_deref(), Some("-2002"));
        assert_eq!(usage.chats[1].kinds.total(), 5);
        assert!(usage.chats[2].chat_id.is_none());
        assert_eq!(usage.chats[2].kinds.other, 3);

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// `days < 0` 全量，永不截止；`0` 不回爬；`> 0` 只保留最近 N 天。
pub fn is_before_cutoff(date: chrono::DateTime<chrono::Utc>, days: i32) -> bool {
    if days < 0 {
        return false;
    }
    if days == 0 {
        return true;
    }
    date < chrono::Utc::now() - chrono::Duration::days(days as i64)
}
