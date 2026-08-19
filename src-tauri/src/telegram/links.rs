use grammers_client::message::Message;
use grammers_client::tl::enums::MessageEntity;

use crate::telegram::store::MessageLink;

pub fn extract_message_links(message: &Message) -> Vec<MessageLink> {
    let Some(entities) = message.fmt_entities() else {
        return Vec::new();
    };
    links_from_entities(message.text(), entities)
}

pub fn links_from_entities(text: &str, entities: &[MessageEntity]) -> Vec<MessageLink> {
    let mut links = Vec::new();
    for entity in entities {
        let (offset, length, raw) = match entity {
            MessageEntity::TextUrl(entity) => (entity.offset, entity.length, entity.url.clone()),
            MessageEntity::Url(entity) => (
                entity.offset,
                entity.length,
                utf16_slice(text, entity.offset, entity.length),
            ),
            _ => continue,
        };
        let Some(url) = sanitize_http_url(&raw) else {
            continue;
        };
        if length <= 0 {
            continue;
        }
        links.push(MessageLink {
            offset,
            length,
            url,
        });
    }
    links.sort_by_key(|link| link.offset);
    links
}

pub fn sanitize_http_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        return None;
    }
    let rest = if lower.starts_with("https://") {
        &url[8..]
    } else {
        &url[7..]
    };
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    Some(url.to_string())
}

fn utf16_slice(text: &str, offset: i32, length: i32) -> String {
    let start = offset.max(0) as usize;
    let len = length.max(0) as usize;
    let units: Vec<u16> = text.encode_utf16().skip(start).take(len).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_https() {
        assert_eq!(
            sanitize_http_url("https://example.com/a"),
            Some("https://example.com/a".into())
        );
        assert!(sanitize_http_url("javascript:alert(1)").is_none());
        assert!(sanitize_http_url("tg://user?id=1").is_none());
        assert!(sanitize_http_url("https://").is_none());
        assert!(sanitize_http_url("https://exa mple.com").is_none());
    }

    #[test]
    fn utf16_handles_emoji() {
        let text = "前🐮后";
        // 🐮 is one UTF-16 surrogate pair (2 units)
        assert_eq!(utf16_slice(text, 1, 2), "🐮");
        assert_eq!(utf16_slice(text, 3, 1), "后");
    }
}
