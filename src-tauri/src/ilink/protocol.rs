use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::error::AppError;

pub const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
// 参考实现已知可用值为 0.1.0 / 1.0.0 / 1.0.2，取官方包版本
const CHANNEL_VERSION: &str = "1.0.2";
const BOT_TYPE: &str = "3";
const QR_TIMEOUT: Duration = Duration::from_secs(20);
const QR_POLL_TIMEOUT: Duration = Duration::from_secs(40);
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const UPDATES_TIMEOUT: Duration = Duration::from_secs(40);

static UIN_SEQ: AtomicU32 = AtomicU32::new(1);

#[derive(Debug)]
pub enum ProtocolError {
    Expired,
    Http(String),
}

impl ProtocolError {
    pub fn message(&self) -> String {
        match self {
            Self::Expired => "微信登录已过期，请重新扫码".into(),
            Self::Http(msg) => msg.clone(),
        }
    }
}

pub struct IlinkHttp {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QrCode {
    pub qrcode: String,
    /// 微信返回的待编码内容（一个 liteapp URL），不是图片本身
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrPoll {
    Wait,
    Scanned,
    Confirmed,
    Expired,
}

#[derive(Debug, Clone)]
pub struct QrLogin {
    pub poll: QrPoll,
    pub bot_token: Option<String>,
    pub bot_id: Option<String>,
    /// 扫码授权的微信机主，用于限定绑定来源
    pub owner_user_id: Option<String>,
    pub baseurl: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub from_user_id: String,
    pub context_token: String,
    pub message_type: i32,
    /// type==1 文本项的内容（bot 指令用），解析不到为 None
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Updates {
    pub messages: Vec<InboundMessage>,
    pub buf: String,
}

enum PostError {
    Timeout,
    Other(String),
}

fn map_post(err: reqwest::Error) -> PostError {
    // reqwest 的 Display 不含 source，超时只能靠类型判断
    if err.is_timeout() {
        PostError::Timeout
    } else {
        PostError::Other(err.to_string())
    }
}

impl IlinkHttp {
    pub fn new(
        proxy_url: Option<&str>,
        base_url: &str,
        token: Option<&str>,
    ) -> Result<Self, AppError> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .connect_timeout(Duration::from_secs(15));
        if let Some(url) = proxy_url.map(str::trim).filter(|url| !url.is_empty()) {
            let proxy = reqwest::Proxy::all(url)
                .map_err(|err| AppError::Config(format!("代理无效: {err}")))?;
            builder = builder.proxy(proxy);
        } else {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|err| AppError::Io(err.to_string()))?;
        Ok(Self {
            http,
            base_url: trim_base(base_url),
            token: nonempty(token),
        })
    }

    pub async fn get_qrcode(&self) -> Result<QrCode, AppError> {
        let url = format!(
            "{}/ilink/bot/get_bot_qrcode?bot_type={}",
            self.base_url, BOT_TYPE
        );
        let resp = self
            .http
            .get(url)
            .timeout(QR_TIMEOUT)
            .send()
            .await
            .map_err(map_reqwest)?;
        let raw = read_body(resp).await?;
        let parsed: QrCodeJson =
            serde_json::from_str(&raw).map_err(|err| AppError::Io(err.to_string()))?;
        let qrcode = parsed.qrcode.unwrap_or_default();
        if qrcode.is_empty() {
            return Err(AppError::Io("微信未返回二维码".into()));
        }
        Ok(QrCode {
            content: parsed.qrcode_img_content.unwrap_or_default(),
            qrcode,
        })
    }

    pub async fn poll_qr_status(&self, qrcode: &str) -> Result<QrLogin, AppError> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            self.base_url,
            urlencoding(qrcode)
        );
        let resp = match self
            .http
            .get(url)
            .header("iLink-App-ClientVersion", "1")
            .timeout(QR_POLL_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.is_timeout() || err.is_connect() => {
                return Ok(QrLogin {
                    poll: QrPoll::Wait,
                    bot_token: None,
                    bot_id: None,
                    owner_user_id: None,
                    baseurl: None,
                });
            }
            Err(err) => return Err(map_reqwest(err)),
        };
        let raw = read_body(resp).await?;
        let parsed: QrStatusJson =
            serde_json::from_str(&raw).map_err(|err| AppError::Io(err.to_string()))?;
        Ok(QrLogin {
            poll: parse_qr_poll(parsed.status.as_deref()),
            bot_token: nonempty(parsed.bot_token.as_deref()),
            bot_id: nonempty(parsed.ilink_bot_id.as_deref()),
            owner_user_id: nonempty(parsed.ilink_user_id.as_deref()),
            baseurl: nonempty(parsed.baseurl.as_deref()),
        })
    }

    pub async fn get_updates(&self, buf: &str) -> Result<Updates, ProtocolError> {
        let body = serde_json::json!({
            "get_updates_buf": buf,
            "base_info": { "channel_version": CHANNEL_VERSION },
        });
        let raw = match self
            .post_json("ilink/bot/getupdates", &body, UPDATES_TIMEOUT)
            .await
        {
            Ok(raw) => raw,
            // 长轮询超时视为一次空返回，游标保持不变
            Err(PostError::Timeout) => {
                return Ok(Updates {
                    messages: Vec::new(),
                    buf: buf.to_string(),
                });
            }
            Err(PostError::Other(msg)) => return Err(ProtocolError::Http(msg)),
        };
        let parsed: UpdatesJson =
            serde_json::from_str(&raw).map_err(|err| ProtocolError::Http(err.to_string()))?;
        check_ret(parsed.ret, parsed.errcode)?;
        let messages = parsed
            .msgs
            .unwrap_or_default()
            .into_iter()
            .filter_map(|msg| {
                let from = nonempty(msg.from_user_id.as_deref())?;
                let token = nonempty(msg.context_token.as_deref())?;
                Some(InboundMessage {
                    from_user_id: from,
                    context_token: token,
                    message_type: msg.message_type.unwrap_or(1),
                    text: extract_text(msg.item_list.as_ref()),
                })
            })
            .collect();
        Ok(Updates {
            messages,
            buf: parsed.get_updates_buf.unwrap_or_else(|| buf.to_string()),
        })
    }

    pub async fn send_text(
        &self,
        to_user_id: &str,
        context_token: &str,
        text: &str,
    ) -> Result<(), ProtocolError> {
        let body = serde_json::json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id(),
                "message_type": 2,
                "message_state": 2,
                "item_list": [{
                    "type": 1,
                    "text_item": { "text": text }
                }],
                "context_token": context_token,
            },
            "base_info": { "channel_version": CHANNEL_VERSION },
        });
        let raw = self
            .post_json("ilink/bot/sendmessage", &body, SEND_TIMEOUT)
            .await
            .map_err(|err| match err {
                PostError::Timeout => ProtocolError::Http("微信请求超时".into()),
                PostError::Other(msg) => ProtocolError::Http(msg),
            })?;
        let parsed: SendJson =
            serde_json::from_str(&raw).map_err(|err| ProtocolError::Http(err.to_string()))?;
        check_ret(parsed.ret, parsed.errcode)
    }

    async fn post_json(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<String, PostError> {
        let url = format!("{}/{endpoint}", self.base_url);
        let payload =
            serde_json::to_string(body).map_err(|err| PostError::Other(err.to_string()))?;
        let mut req = self
            .http
            .post(url)
            .timeout(timeout)
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("X-WECHAT-UIN", wechat_uin())
            .body(payload);
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await.map_err(map_post)?;
        let status = resp.status();
        let raw = resp.text().await.map_err(map_post)?;
        if !status.is_success() {
            return Err(PostError::Other(format!("HTTP {status}")));
        }
        Ok(raw)
    }
}

fn check_ret(ret: Option<i64>, errcode: Option<i64>) -> Result<(), ProtocolError> {
    let code = errcode
        .filter(|code| *code != 0)
        .or(ret.filter(|code| *code != 0));
    match code {
        None => Ok(()),
        Some(-14) => Err(ProtocolError::Expired),
        // -2 等非零码按规范是参数错误，不单独分类
        Some(code) => Err(ProtocolError::Http(format!("微信接口错误 {code}"))),
    }
}

fn parse_qr_poll(status: Option<&str>) -> QrPoll {
    match status.unwrap_or("wait") {
        "scaned" | "scanned" => QrPoll::Scanned,
        "confirmed" => QrPoll::Confirmed,
        "expired" => QrPoll::Expired,
        _ => QrPoll::Wait,
    }
}

fn trim_base(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        url.to_string()
    }
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn map_reqwest(err: reqwest::Error) -> AppError {
    AppError::Io(err.to_string())
}

async fn read_body(resp: reqwest::Response) -> Result<String, AppError> {
    let status = resp.status();
    let raw = resp.text().await.map_err(map_reqwest)?;
    if !status.is_success() {
        return Err(AppError::Io(format!("HTTP {status}")));
    }
    Ok(raw)
}

fn client_id() -> String {
    let ms = now_millis();
    format!("tgd:{ms}-{:08x}", next_u32())
}

fn wechat_uin() -> String {
    encode_base64(next_u32().to_string().as_bytes())
}

fn next_u32() -> u32 {
    let seq = UIN_SEQ.fetch_add(1, Ordering::Relaxed);
    now_millis() as u32 ^ seq.wrapping_mul(1664525)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn urlencoding(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let remain = data.len() - i;
        let b0 = data[i];
        let b1 = if remain > 1 { data[i + 1] } else { 0 };
        let b2 = if remain > 2 { data[i + 2] } else { 0 };
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if remain > 1 {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if remain > 2 {
            out.push(TABLE[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

/// 把微信返回的待编码内容渲染成二维码 SVG data URI，前端 `<img>` 可直接用
pub(crate) fn qr_data_uri(content: &str) -> Option<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    if content.trim().is_empty() {
        return None;
    }
    let code = QrCode::new(content.as_bytes()).ok()?;
    let image = code
        .render::<svg::Color>()
        .min_dimensions(256, 256)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Some(format!(
        "data:image/svg+xml;base64,{}",
        encode_base64(image.as_bytes())
    ))
}

#[derive(Deserialize)]
struct QrCodeJson {
    qrcode: Option<String>,
    qrcode_img_content: Option<String>,
}

#[derive(Deserialize)]
struct QrStatusJson {
    status: Option<String>,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    ilink_user_id: Option<String>,
    baseurl: Option<String>,
}

#[derive(Deserialize)]
struct UpdatesJson {
    ret: Option<i64>,
    errcode: Option<i64>,
    msgs: Option<Vec<MessageJson>>,
    get_updates_buf: Option<String>,
}

#[derive(Deserialize)]
struct MessageJson {
    from_user_id: Option<String>,
    context_token: Option<String>,
    message_type: Option<i32>,
    item_list: Option<Vec<InboundItemJson>>,
}

#[derive(Deserialize)]
struct InboundItemJson {
    #[serde(rename = "type")]
    kind: Option<i32>,
    text_item: Option<InboundTextItemJson>,
}

#[derive(Deserialize)]
struct InboundTextItemJson {
    text: Option<String>,
}

/// 取 item_list 里 type==1 的文本，多条按换行拼接
fn extract_text(items: Option<&Vec<InboundItemJson>>) -> Option<String> {
    let items = items?;
    let texts: Vec<&str> = items
        .iter()
        .filter(|item| item.kind.unwrap_or(1) == 1)
        .filter_map(|item| item.text_item.as_ref())
        .filter_map(|item| item.text.as_deref())
        .filter(|text| !text.trim().is_empty())
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

#[derive(Deserialize)]
struct SendJson {
    ret: Option<i64>,
    errcode: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_poll_maps_typo() {
        assert_eq!(parse_qr_poll(Some("scaned")), QrPoll::Scanned);
        assert_eq!(parse_qr_poll(Some("confirmed")), QrPoll::Confirmed);
        assert_eq!(parse_qr_poll(Some("expired")), QrPoll::Expired);
        assert_eq!(parse_qr_poll(None), QrPoll::Wait);
    }

    #[test]
    fn ret_codes() {
        assert!(check_ret(Some(0), Some(0)).is_ok());
        assert!(matches!(
            check_ret(Some(-14), None),
            Err(ProtocolError::Expired)
        ));
        // -2 是参数错误，按通用错误处理
        assert!(matches!(
            check_ret(None, Some(-2)),
            Err(ProtocolError::Http(msg)) if msg.contains("-2")
        ));
    }

    #[test]
    fn base64_uin_roundtrip_len() {
        let encoded = encode_base64(b"123456");
        assert_eq!(encoded, "MTIzNDU2");
    }

    #[test]
    fn qr_data_uri_renders_svg() {
        let uri = qr_data_uri("https://liteapp.weixin.qq.com/q/abc?bot_type=3").unwrap();
        assert!(uri.starts_with("data:image/svg+xml;base64,"));
        assert!(uri.len() > "data:image/svg+xml;base64,".len());
        assert!(qr_data_uri("  ").is_none());
    }

    #[test]
    fn inbound_text_extracted_from_item_list() {
        let msg: MessageJson = serde_json::from_str(
            r#"{"from_user_id":"u","context_token":"c","item_list":[{"type":1,"text_item":{"text":"/暂停下载"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            extract_text(msg.item_list.as_ref()).as_deref(),
            Some("/暂停下载")
        );

        let mixed: MessageJson = serde_json::from_str(
            r#"{"item_list":[{"type":2,"text_item":{"text":"忽略"}},{"type":1,"text_item":{"text":"a"}},{"type":1,"text_item":{"text":"b"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            extract_text(mixed.item_list.as_ref()).as_deref(),
            Some("a\nb")
        );

        let empty: MessageJson = serde_json::from_str("{}").unwrap();
        assert!(extract_text(empty.item_list.as_ref()).is_none());

        let blank: MessageJson =
            serde_json::from_str(r#"{"item_list":[{"type":1,"text_item":{"text":"   "}}]}"#)
                .unwrap();
        assert!(extract_text(blank.item_list.as_ref()).is_none());
    }
}
