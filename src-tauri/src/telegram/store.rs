use std::path::Path;

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error::AppError;

const SCHEMA: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS messages (
        chat_id TEXT NOT NULL,
        message_id INTEGER NOT NULL,
        date_unix INTEGER NOT NULL,
        sender TEXT NOT NULL DEFAULT '',
        text TEXT NOT NULL DEFAULT '',
        media_kind TEXT,
        media_file_id TEXT,
        PRIMARY KEY (chat_id, message_id)
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_messages_chat_date
        ON messages (chat_id, date_unix)
    "#,
    r#"
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        sender,
        text,
        content='messages',
        content_rowid='rowid'
    )
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, sender, text)
        VALUES (new.rowid, new.sender, new.text);
    END
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, sender, text)
        VALUES ('delete', old.rowid, old.sender, old.text);
    END
    "#,
    r#"
    CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, sender, text)
        VALUES ('delete', old.rowid, old.sender, old.text);
        INSERT INTO messages_fts(rowid, sender, text)
        VALUES (new.rowid, new.sender, new.text);
    END
    "#,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MessageLink {
    pub offset: i32,
    pub length: i32,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub chat_id: String,
    pub message_id: i32,
    pub date_unix: i64,
    pub sender: String,
    pub text: String,
    pub media_kind: Option<String>,
    pub media_file_id: Option<String>,
    pub links: Vec<MessageLink>,
}

#[derive(Clone)]
pub struct MessageStore {
    pool: SqlitePool,
}

impl MessageStore {
    pub fn db_path(root: &Path) -> std::path::PathBuf {
        root.join("messages.db")
    }

    pub async fn open(root: &Path) -> Result<Self, AppError> {
        std::fs::create_dir_all(root).map_err(|err| AppError::Io(err.to_string()))?;
        let options = SqliteConnectOptions::new()
            .filename(Self::db_path(root))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<(), AppError> {
        for statement in SCHEMA {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .map_err(|err| AppError::Io(err.to_string()))?;
        }
        self.ensure_column(
            "messages",
            "links",
            "ALTER TABLE messages ADD COLUMN links TEXT NOT NULL DEFAULT '[]'",
        )
        .await?;
        Ok(())
    }

    async fn ensure_column(&self, table: &str, column: &str, ddl: &str) -> Result<(), AppError> {
        let rows = sqlx::query("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(&self.pool)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        let exists = rows
            .iter()
            .any(|row| row.get::<String, _>(0).eq_ignore_ascii_case(column));
        if !exists {
            sqlx::query(ddl)
                .execute(&self.pool)
                .await
                .map_err(|err| AppError::Io(err.to_string()))?;
        }
        Ok(())
    }

    /// 插入一条消息。已存在 `(chat_id, message_id)` 时返回 `false`，不覆盖。
    pub async fn insert(&self, record: &MessageRecord) -> Result<bool, AppError> {
        let result = sqlx::query(
            r#"
            INSERT INTO messages (
                chat_id, message_id, date_unix, sender, text, media_kind, media_file_id, links
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(chat_id, message_id) DO NOTHING
            "#,
        )
        .bind(&record.chat_id)
        .bind(record.message_id)
        .bind(record.date_unix)
        .bind(&record.sender)
        .bind(&record.text)
        .bind(&record.media_kind)
        .bind(&record.media_file_id)
        .bind(links_json(&record.links))
        .execute(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(result.rows_affected() > 0)
    }

    /// 新消息插入；已存在且 `links` 为空时补上链接，不改正文。
    pub async fn upsert(&self, record: &MessageRecord) -> Result<bool, AppError> {
        let existed = self.exists(&record.chat_id, record.message_id).await?;
        if !existed {
            return self.insert(record).await;
        }
        if !record.links.is_empty() {
            sqlx::query(
                r#"
                UPDATE messages
                SET links = ?
                WHERE chat_id = ? AND message_id = ?
                  AND TRIM(COALESCE(links, '')) IN ('', '[]')
                "#,
            )
            .bind(links_json(&record.links))
            .bind(&record.chat_id)
            .bind(record.message_id)
            .execute(&self.pool)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        }
        Ok(false)
    }

    pub async fn delete_chat(&self, chat_id: &str) -> Result<u32, AppError> {
        let result = sqlx::query("DELETE FROM messages WHERE chat_id = ?")
            .bind(chat_id)
            .execute(&self.pool)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(u32::try_from(result.rows_affected()).unwrap_or(u32::MAX))
    }

    pub async fn exists(&self, chat_id: &str, message_id: i32) -> Result<bool, AppError> {
        let row =
            sqlx::query("SELECT 1 FROM messages WHERE chat_id = ? AND message_id = ? LIMIT 1")
                .bind(chat_id)
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(row.is_some())
    }

    pub async fn get(
        &self,
        chat_id: &str,
        message_id: i32,
    ) -> Result<Option<MessageRecord>, AppError> {
        let row = sqlx::query(
            r#"
            SELECT chat_id, message_id, date_unix, sender, text, media_kind, media_file_id, links
            FROM messages
            WHERE chat_id = ? AND message_id = ?
            LIMIT 1
            "#,
        )
        .bind(chat_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(row.map(record_from_row))
    }

    /// 按会话列出消息，`message_id` 倒序。`query` 非空时先 FTS 再回退 LIKE。
    pub async fn list_chat(
        &self,
        chat_id: &str,
        query: Option<&str>,
        before_message_id: Option<i32>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let limit = limit.clamp(1, 201);
        let query = query.map(str::trim).filter(|text| !text.is_empty());
        if let Some(query) = query {
            match self
                .search_chat_fts(chat_id, query, before_message_id, limit)
                .await
            {
                Ok(rows) if !rows.is_empty() => return Ok(rows),
                Ok(_) => {}
                Err(err) => log::warn!("fts search fallback to like: {err}"),
            }
            return self
                .search_chat_like(chat_id, query, before_message_id, limit)
                .await;
        }
        self.page_chat(chat_id, before_message_id, limit).await
    }

    async fn page_chat(
        &self,
        chat_id: &str,
        before_message_id: Option<i32>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT chat_id, message_id, date_unix, sender, text, media_kind, media_file_id, links
            FROM messages
            WHERE chat_id = ?
              AND (? IS NULL OR message_id < ?)
            ORDER BY message_id DESC
            LIMIT ?
            "#,
        )
        .bind(chat_id)
        .bind(before_message_id)
        .bind(before_message_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    async fn search_chat_fts(
        &self,
        chat_id: &str,
        query: &str,
        before_message_id: Option<i32>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT m.chat_id, m.message_id, m.date_unix, m.sender, m.text, m.media_kind, m.media_file_id, m.links
            FROM messages m
            JOIN messages_fts f ON f.rowid = m.rowid
            WHERE m.chat_id = ?
              AND messages_fts MATCH ?
              AND (? IS NULL OR m.message_id < ?)
            ORDER BY m.message_id DESC
            LIMIT ?
            "#,
        )
        .bind(chat_id)
        .bind(fts_match_query(query))
        .bind(before_message_id)
        .bind(before_message_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    async fn search_chat_like(
        &self,
        chat_id: &str,
        query: &str,
        before_message_id: Option<i32>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let pattern = like_pattern(query);
        let rows = sqlx::query(
            r#"
            SELECT chat_id, message_id, date_unix, sender, text, media_kind, media_file_id, links
            FROM messages
            WHERE chat_id = ?
              AND (text LIKE ? ESCAPE '\' OR sender LIKE ? ESCAPE '\')
              AND (? IS NULL OR message_id < ?)
            ORDER BY message_id DESC
            LIMIT ?
            "#,
        )
        .bind(chat_id)
        .bind(&pattern)
        .bind(&pattern)
        .bind(before_message_id)
        .bind(before_message_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    /// 跨会话检索。`query` 为空返回空；FTS 无命中或失败时回退 LIKE。
    pub async fn search_all(
        &self,
        query: &str,
        before: Option<&MessageSearchCursor>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 201);
        match self.search_all_fts(query, before, limit).await {
            Ok(rows) if !rows.is_empty() => return Ok(rows),
            Ok(_) => {}
            Err(err) => log::warn!("fts search_all fallback to like: {err}"),
        }
        self.search_all_like(query, before, limit).await
    }

    /// 按媒体类型列出 message_id，新到旧。用于类型定点回补。
    pub async fn list_media_ids(
        &self,
        chat_id: &str,
        kinds: &[&str],
        min_date: Option<i64>,
        min_id: Option<i32>,
        before_id: Option<i32>,
        limit: i32,
    ) -> Result<Vec<i32>, AppError> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 100);
        let k0 = kinds.first().copied();
        let k1 = kinds.get(1).copied();
        let k2 = kinds.get(2).copied();
        let k3 = kinds.get(3).copied();
        let rows = sqlx::query(
            r#"
            SELECT message_id
            FROM messages
            WHERE chat_id = ?
              AND (
                media_kind = ?
                OR media_kind = ?
                OR media_kind = ?
                OR media_kind = ?
              )
              AND (? IS NULL OR date_unix >= ?)
              AND (? IS NULL OR message_id > ?)
              AND (? IS NULL OR message_id < ?)
            ORDER BY message_id DESC
            LIMIT ?
            "#,
        )
        .bind(chat_id)
        .bind(k0)
        .bind(k1)
        .bind(k2)
        .bind(k3)
        .bind(min_date)
        .bind(min_date)
        .bind(min_id)
        .bind(min_id)
        .bind(before_id)
        .bind(before_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(|row| row.get("message_id")).collect())
    }

    pub async fn oldest_date_unix(&self, chat_id: &str) -> Result<Option<i64>, AppError> {
        let row = sqlx::query("SELECT MIN(date_unix) AS oldest FROM messages WHERE chat_id = ?")
            .bind(chat_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(row.and_then(|row| row.get::<Option<i64>, _>("oldest")))
    }

    pub async fn newest_message_id(&self, chat_id: &str) -> Result<Option<i32>, AppError> {
        let row = sqlx::query("SELECT MAX(message_id) AS newest FROM messages WHERE chat_id = ?")
            .bind(chat_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(row.and_then(|row| row.get::<Option<i32>, _>("newest")))
    }

    async fn search_all_fts(
        &self,
        query: &str,
        before: Option<&MessageSearchCursor>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let (before_date, before_chat, before_msg) = cursor_binds(before);
        let rows = sqlx::query(
            r#"
            SELECT m.chat_id, m.message_id, m.date_unix, m.sender, m.text, m.media_kind, m.media_file_id, m.links
            FROM messages m
            JOIN messages_fts f ON f.rowid = m.rowid
            WHERE messages_fts MATCH ?
              AND (
                ? IS NULL
                OR m.date_unix < ?
                OR (m.date_unix = ? AND m.chat_id < ?)
                OR (m.date_unix = ? AND m.chat_id = ? AND m.message_id < ?)
              )
            ORDER BY m.date_unix DESC, m.chat_id DESC, m.message_id DESC
            LIMIT ?
            "#,
        )
        .bind(fts_match_query(query))
        .bind(before_date)
        .bind(before_date)
        .bind(before_date)
        .bind(before_chat.as_deref())
        .bind(before_date)
        .bind(before_chat.as_deref())
        .bind(before_msg)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    async fn search_all_like(
        &self,
        query: &str,
        before: Option<&MessageSearchCursor>,
        limit: i32,
    ) -> Result<Vec<MessageRecord>, AppError> {
        let pattern = like_pattern(query);
        let (before_date, before_chat, before_msg) = cursor_binds(before);
        let rows = sqlx::query(
            r#"
            SELECT chat_id, message_id, date_unix, sender, text, media_kind, media_file_id, links
            FROM messages
            WHERE (text LIKE ? ESCAPE '\' OR sender LIKE ? ESCAPE '\')
              AND (
                ? IS NULL
                OR date_unix < ?
                OR (date_unix = ? AND chat_id < ?)
                OR (date_unix = ? AND chat_id = ? AND message_id < ?)
              )
            ORDER BY date_unix DESC, chat_id DESC, message_id DESC
            LIMIT ?
            "#,
        )
        .bind(&pattern)
        .bind(&pattern)
        .bind(before_date)
        .bind(before_date)
        .bind(before_date)
        .bind(before_chat.as_deref())
        .bind(before_date)
        .bind(before_chat.as_deref())
        .bind(before_msg)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }
}

#[derive(Debug, Clone)]
pub struct MessageSearchCursor {
    pub date_unix: i64,
    pub chat_id: String,
    pub message_id: i32,
}

fn cursor_binds(
    before: Option<&MessageSearchCursor>,
) -> (Option<i64>, Option<String>, Option<i32>) {
    match before {
        Some(cursor) => (
            Some(cursor.date_unix),
            Some(cursor.chat_id.clone()),
            Some(cursor.message_id),
        ),
        None => (None, None, None),
    }
}

fn record_from_row(row: sqlx::sqlite::SqliteRow) -> MessageRecord {
    let links_raw: String = row
        .try_get("links")
        .or_else(|_| row.try_get(7))
        .unwrap_or_else(|_| "[]".into());
    MessageRecord {
        chat_id: row.get("chat_id"),
        message_id: row.get("message_id"),
        date_unix: row.get("date_unix"),
        sender: row.get("sender"),
        text: row.get("text"),
        media_kind: row.get("media_kind"),
        media_file_id: row.get("media_file_id"),
        links: parse_links(&links_raw),
    }
}

fn parse_links(raw: &str) -> Vec<MessageLink> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn links_json(links: &[MessageLink]) -> String {
    serde_json::to_string(links).unwrap_or_else(|_| "[]".into())
}

fn fts_match_query(raw: &str) -> String {
    format!("\"{}\"", raw.replace('"', "\"\""))
}

fn like_pattern(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_store() -> (MessageStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "tgd-msg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = MessageStore::open(&root).await.unwrap();
        (store, root)
    }

    fn rec(chat: &str, id: i32, text: &str) -> MessageRecord {
        MessageRecord {
            chat_id: chat.into(),
            message_id: id,
            date_unix: 1_700_000_000,
            sender: "Alice".into(),
            text: text.into(),
            media_kind: None,
            media_file_id: None,
            links: Vec::new(),
        }
    }

    fn rec_link(chat: &str, id: i32, text: &str, url: &str) -> MessageRecord {
        let mut record = rec(chat, id, text);
        record.links = vec![MessageLink {
            offset: 0,
            length: text.encode_utf16().count() as i32,
            url: url.into(),
        }];
        record
    }

    #[tokio::test]
    async fn unique_chat_and_message_id() {
        let (store, root) = temp_store().await;
        let first = rec("-1001", 42, "hello world");
        assert!(store.insert(&first).await.unwrap());
        assert!(store.exists("-1001", 42).await.unwrap());
        assert!(!store.insert(&first).await.unwrap());

        let other_chat = rec("-1002", 42, "hello world");
        assert!(store.insert(&other_chat).await.unwrap());

        let hits = store.search_all("hello", None, 10).await.unwrap();
        assert_eq!(hits.len(), 2);

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn fts_does_not_match_unrelated() {
        let (store, root) = temp_store().await;
        store.insert(&rec("1", 1, "alpha beta")).await.unwrap();
        store.insert(&rec("1", 2, "gamma")).await.unwrap();
        let hits = store.search_all("alpha", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, 1);
        assert!(store.search_all("zzz", None, 10).await.unwrap().is_empty());
        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn list_chat_newest_first_and_cursor() {
        let (store, root) = temp_store().await;
        store.insert(&rec("1", 1, "a")).await.unwrap();
        store.insert(&rec("1", 2, "b")).await.unwrap();
        store.insert(&rec("1", 3, "c")).await.unwrap();
        store.insert(&rec("2", 4, "other")).await.unwrap();

        let page = store.list_chat("1", None, None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![3, 2]
        );

        let next = store.list_chat("1", None, Some(2), 2).await.unwrap();
        assert_eq!(
            next.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![1]
        );

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn search_stays_in_chat() {
        let (store, root) = temp_store().await;
        store.insert(&rec("1", 1, "hello alpha")).await.unwrap();
        store.insert(&rec("2", 2, "hello alpha")).await.unwrap();
        let hits = store.list_chat("1", Some("hello"), None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chat_id, "1");

        store.insert(&rec("1", 3, "今天天气不错")).await.unwrap();
        let cjk = store.list_chat("1", Some("天气"), None, 10).await.unwrap();
        assert_eq!(cjk.len(), 1);
        assert_eq!(cjk[0].message_id, 3);

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn upsert_fills_empty_links_only() {
        let (store, root) = temp_store().await;
        let with_links = rec_link("2", 9, "原文链接", "https://example.com/seed");
        assert!(store.insert(&with_links).await.unwrap());
        let seeded = store.list_chat("2", None, None, 10).await.unwrap();
        assert_eq!(
            seeded[0].links.first().map(|l| l.url.as_str()),
            Some("https://example.com/seed"),
            "direct insert links={:?}",
            seeded[0].links
        );

        store.insert(&rec("1", 1, "原文链接")).await.unwrap();
        let filled = rec_link("1", 1, "原文链接", "https://example.com/a");
        assert!(!store.upsert(&filled).await.unwrap());
        let page = store.list_chat("1", None, None, 10).await.unwrap();
        assert_eq!(
            page[0].links.first().map(|l| l.url.as_str()),
            Some("https://example.com/a"),
            "upsert links={:?}",
            page[0].links
        );

        let other = rec_link("1", 1, "原文链接", "https://example.com/b");
        store.upsert(&other).await.unwrap();
        let page = store.list_chat("1", None, None, 10).await.unwrap();
        assert_eq!(page[0].links[0].url, "https://example.com/a");
        assert_eq!(page[0].text, "原文链接");

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn delete_chat_only_that_chat() {
        let (store, root) = temp_store().await;
        store.insert(&rec("1", 1, "a")).await.unwrap();
        store.insert(&rec("1", 2, "b")).await.unwrap();
        store.insert(&rec("2", 1, "c")).await.unwrap();
        assert_eq!(store.delete_chat("1").await.unwrap(), 2);
        assert!(store
            .list_chat("1", None, None, 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.list_chat("2", None, None, 10).await.unwrap().len(), 1);

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn list_media_ids_filters_kind_date_and_id() {
        let (store, root) = temp_store().await;
        let mut photo = rec("1", 10, "");
        photo.media_kind = Some("photo".into());
        photo.date_unix = 1_000;
        let mut video = rec("1", 20, "");
        video.media_kind = Some("video".into());
        video.date_unix = 2_000;
        let mut old = rec("1", 30, "");
        old.media_kind = Some("photo".into());
        old.date_unix = 100;
        let mut other = rec("2", 40, "");
        other.media_kind = Some("photo".into());
        other.date_unix = 2_000;
        store.insert(&photo).await.unwrap();
        store.insert(&video).await.unwrap();
        store.insert(&old).await.unwrap();
        store.insert(&other).await.unwrap();

        let ids = store
            .list_media_ids("1", &["photo"], Some(500), None, None, 10)
            .await
            .unwrap();
        assert_eq!(ids, vec![10]);

        let after = store
            .list_media_ids("1", &["photo", "video"], None, Some(10), None, 10)
            .await
            .unwrap();
        assert_eq!(after, vec![30, 20]);

        let page = store
            .list_media_ids("1", &["photo", "video"], None, None, Some(30), 10)
            .await
            .unwrap();
        assert_eq!(page, vec![20, 10]);

        assert!(store
            .list_media_ids("1", &[], None, None, None, 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.oldest_date_unix("1").await.unwrap(), Some(100));
        assert_eq!(store.oldest_date_unix("missing").await.unwrap(), None);
        assert_eq!(store.newest_message_id("1").await.unwrap(), Some(30));
        assert_eq!(store.newest_message_id("missing").await.unwrap(), None);

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn search_all_cross_chat_and_cursor() {
        let (store, root) = temp_store().await;
        let mut a = rec("1", 1, "hello alpha");
        a.date_unix = 100;
        let mut b = rec("2", 2, "hello beta");
        b.date_unix = 200;
        let mut c = rec("1", 3, "hello gamma");
        c.date_unix = 300;
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        store.insert(&c).await.unwrap();

        let page = store.search_all("hello", None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![3, 2]
        );

        let next = store
            .search_all(
                "hello",
                Some(&MessageSearchCursor {
                    date_unix: page[1].date_unix,
                    chat_id: page[1].chat_id.clone(),
                    message_id: page[1].message_id,
                }),
                2,
            )
            .await
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].message_id, 1);

        store.insert(&rec("1", 4, "今天天气不错")).await.unwrap();
        let cjk = store.search_all("天气", None, 10).await.unwrap();
        assert_eq!(cjk.len(), 1);
        assert_eq!(cjk[0].message_id, 4);
        assert!(store.search_all("   ", None, 10).await.unwrap().is_empty());

        store.pool.close().await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
