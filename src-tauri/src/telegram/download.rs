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

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "photo" => Some(Self::Photo),
            "video" => Some(Self::Video),
            "document" => Some(Self::Document),
            "audio" => Some(Self::Audio),
            _ => None,
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

pub(crate) fn part_off_path(part: &Path) -> PathBuf {
    let mut tmp = part.as_os_str().to_os_string();
    tmp.push(".off");
    PathBuf::from(tmp)
}

/// 写续传游标。并行预分配过的 `.part` 靠它区分真实进度，
/// 写失败必须让调用方中止下载，否则 off 丢失后会拿预分配长度当进度。
pub(crate) fn write_part_off(part: &Path, offset: u64) -> std::io::Result<()> {
    std::fs::write(part_off_path(part), offset.to_string())
}

pub(crate) fn clear_part_off(part: &Path) {
    let _ = std::fs::remove_file(part_off_path(part));
}

/// 丢掉未完成下载：`.part` 和 `.part.off`。跳过文件时用；暂停/退出不走这里。
pub(crate) fn discard_partial(dest: &Path) {
    let part = part_path(dest);
    let _ = std::fs::remove_file(&part);
    clear_part_off(&part);
}

/// 续传起点。并行预分配文件的长度恒为总量，真实进度只认 `.part.off`；
/// 该文件从建起就保证 off 已成功写入（写失败会中止下载）。
pub(crate) fn part_resume_len(part: &Path) -> u64 {
    if !part.exists() {
        return 0;
    }
    if let Ok(raw) = std::fs::read_to_string(part_off_path(part)) {
        if let Ok(offset) = raw.trim().parse::<u64>() {
            return aligned_part_len(offset);
        }
    }
    std::fs::metadata(part)
        .map(|meta| aligned_part_len(meta.len()))
        .unwrap_or(0)
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
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.ends_with(".part.off") {
            return;
        }
        let is_part = name.ends_with(".part");
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

/// 删掉该会话媒体目录和索引里记下的文件（含 `.part`）。
pub fn delete_chat_media(download_dir: &Path, chat_id: &str, extra_paths: &[PathBuf]) {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        return;
    }
    for path in extra_paths {
        let _ = std::fs::remove_file(path);
        let part = part_path(path);
        let _ = std::fs::remove_file(&part);
        clear_part_off(&part);
    }
    let Ok(entries) = std::fs::read_dir(download_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.rsplit_once('_').is_some_and(|(_, id)| id == chat_id) {
            let _ = std::fs::remove_dir_all(&path);
        }
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
    /// 「清除并重爬」会 `take_chat` 并删掉磁盘文件，从 0 重下。
    pub fn contains(&self, file_id: &str) -> bool {
        self.files.contains_key(file_id)
    }

    /// 丢掉一条索引，强制重下时用。
    pub fn forget(&mut self, file_id: &str) -> Option<MediaIndexEntry> {
        self.files.remove(file_id)
    }

    fn belongs_to_chat(entry: &MediaIndexEntry, chat_id: &str) -> bool {
        entry.chat_id.as_deref() == Some(chat_id)
            || infer_from_path(&entry.path).chat_id.as_deref() == Some(chat_id)
    }

    /// 丢掉该会话里磁盘上已经不在的索引。文件还在的留下，重爬时跳过。
    #[cfg(test)]
    pub fn forget_missing_for_chat(&mut self, chat_id: &str) -> usize {
        let before = self.files.len();
        self.files.retain(|_, entry| {
            if !Self::belongs_to_chat(entry, chat_id) {
                return true;
            }
            entry.path.is_file()
        });
        before.saturating_sub(self.files.len())
    }

    /// 丢掉该会话全部索引，返回路径以便删文件。
    pub fn take_chat(&mut self, chat_id: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        self.files.retain(|_, entry| {
            if Self::belongs_to_chat(entry, chat_id) {
                paths.push(entry.path.clone());
                false
            } else {
                true
            }
        });
        paths
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

        let keep = root.join("keep.bin");
        std::fs::write(&keep, b"ok").unwrap();
        let mut mixed = MediaIndex::default();
        mixed.remember(
            "keep".into(),
            keep.clone(),
            MediaKind::Document,
            Some(2),
            Some("-1001".into()),
            Some("Hello".into()),
        );
        mixed.remember(
            "gone".into(),
            root.join("missing.bin"),
            MediaKind::Photo,
            None,
            Some("-1001".into()),
            Some("Hello".into()),
        );
        mixed.remember(
            "other".into(),
            root.join("other-missing.bin"),
            MediaKind::Photo,
            None,
            Some("-1002".into()),
            Some("Other".into()),
        );
        assert_eq!(mixed.forget_missing_for_chat("-1001"), 1);
        assert!(mixed.contains("keep"));
        assert!(!mixed.contains("gone"));
        assert!(mixed.contains("other"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn take_chat_and_delete_media_dir() {
        let root = std::env::temp_dir().join(format!(
            "tgd-wipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let chat_dir = root.join("Hello_-1001/video");
        let other = root.join("Other_-2002/audio");
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let keep = chat_dir.join("a.mp4");
        let part = chat_dir.join("a.mp4.part");
        std::fs::write(&keep, b"data").unwrap();
        std::fs::write(&part, b"part").unwrap();
        std::fs::write(other.join("c.ogg"), b"x").unwrap();

        let mut index = MediaIndex::default();
        index.remember(
            "a".into(),
            keep.clone(),
            MediaKind::Video,
            Some(4),
            Some("-1001".into()),
            Some("Hello".into()),
        );
        index.remember(
            "b".into(),
            other.join("c.ogg"),
            MediaKind::Audio,
            Some(1),
            Some("-2002".into()),
            Some("Other".into()),
        );
        let taken = index.take_chat("-1001");
        assert_eq!(taken, vec![keep.clone()]);
        assert!(!index.contains("a"));
        assert!(index.contains("b"));

        delete_chat_media(&root, "-1001", &taken);
        assert!(!keep.exists());
        assert!(!part.exists());
        assert!(!root.join("Hello_-1001").exists());
        assert!(other.join("c.ogg").exists());

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
    fn discard_partial_removes_part_and_off() {
        let root = std::env::temp_dir().join(format!(
            "tgd-discard-part-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dest = root.join("clip.mp4");
        let part = part_path(&dest);
        std::fs::write(&part, b"partial").unwrap();
        write_part_off(&part, 512).unwrap();
        assert!(part.exists());
        assert!(part_off_path(&part).exists());
        discard_partial(&dest);
        assert!(!part.exists());
        assert!(!part_off_path(&part).exists());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn part_resume_prefers_off_over_preallocated_len() {
        let root = std::env::temp_dir().join(format!(
            "tgd-part-off-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let part = root.join("clip.mp4.part");
        std::fs::write(&part, vec![0u8; (DOWNLOAD_CHUNK * 4) as usize]).unwrap();
        assert_eq!(part_resume_len(&part), DOWNLOAD_CHUNK * 4);
        write_part_off(&part, DOWNLOAD_CHUNK + 10).unwrap();
        assert_eq!(part_resume_len(&part), DOWNLOAD_CHUNK);
        clear_part_off(&part);
        assert_eq!(part_resume_len(&part), DOWNLOAD_CHUNK * 4);
        let _ = std::fs::remove_dir_all(&root);
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

    #[test]
    fn media_kind_from_name_roundtrip() {
        for kind in [
            MediaKind::Photo,
            MediaKind::Video,
            MediaKind::Audio,
            MediaKind::Document,
        ] {
            assert_eq!(MediaKind::from_name(kind.as_str()), Some(kind));
        }
        assert_eq!(MediaKind::from_name("sticker"), None);
    }

    #[test]
    fn below_min_media_size_skips_known_small() {
        assert!(!below_min_media_size(Some(100), 0.0));
        assert!(!below_min_media_size(None, 1.0));
        assert!(below_min_media_size(Some(1024 * 1024 - 1), 1.0));
        assert!(!below_min_media_size(Some(1024 * 1024), 1.0));
        assert_eq!(min_media_bytes(0.0), 0);
        assert_eq!(min_media_bytes(1.0), 1024 * 1024);
    }

    #[test]
    fn forget_removes_index_entry() {
        let mut index = MediaIndex::default();
        index.remember(
            "1".into(),
            PathBuf::from("/tmp/a.jpg"),
            MediaKind::Photo,
            Some(8),
            Some("-1".into()),
            Some("Chat".into()),
        );
        assert!(index.contains("1"));
        assert!(index.forget("1").is_some());
        assert!(!index.contains("1"));
        assert!(index.forget("1").is_none());
    }
}

/// 设置里的 MB 转字节。`<= 0` 或非数字视为不过滤。
pub fn min_media_bytes(min_mb: f64) -> u64 {
    if !min_mb.is_finite() || min_mb <= 0.0 {
        return 0;
    }
    let bytes = min_mb * 1024.0 * 1024.0;
    if bytes >= u64::MAX as f64 {
        u64::MAX
    } else {
        bytes.round() as u64
    }
}

/// 已知体积且小于阈值才跳过；未知体积仍下载。
pub fn below_min_media_size(size: Option<u64>, min_mb: f64) -> bool {
    let min = min_media_bytes(min_mb);
    min > 0 && size.is_some_and(|n| n < min)
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
