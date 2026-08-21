use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use grammers_client::media::{Downloadable, Media};
use grammers_client::sender::{connect, ServerAddr};
use grammers_client::tl;
use grammers_client::{Client, InvocationError};
use grammers_mtproto::transport;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::telegram::download::DOWNLOAD_CHUNK;

/// 额外 MTProto 连接数。每条大约 1~2 MB/s，8 条远低于 300Mbps 出口。
pub const MEDIA_LANES: usize = 8;
/// 小于这个的文件走主连接串行，避免握手开销。
pub const PARALLEL_MIN_SIZE: u64 = 4 * 1024 * 1024;

type EncryptedSender =
    grammers_client::sender::Sender<transport::Full, grammers_mtproto::mtp::Encrypted>;

pub enum ParallelError {
    Flood(u64),
    Cancelled { keep_part: bool },
    Other(String),
}

#[derive(Clone, Debug)]
struct DcAddr {
    id: i32,
    ipv6: bool,
    media_only: bool,
    cdn: bool,
    tcpo_only: bool,
    ip: String,
    port: i32,
}

pub struct MediaPool {
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    dc_id: Option<i32>,
    lanes: Vec<Arc<Mutex<EncryptedSender>>>,
}

impl MediaPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(PoolInner {
                dc_id: None,
                lanes: Vec::new(),
            }),
        })
    }

    pub fn should_parallel(total: u64) -> bool {
        total >= PARALLEL_MIN_SIZE
    }

    pub async fn download(
        &self,
        client: &Client,
        api_id: i32,
        proxy_url: Option<String>,
        media: &Media,
        tmp: &std::path::Path,
        start: u64,
        total: u64,
        cancelled: impl Fn() -> bool + Send + Sync,
        keep_part: impl Fn() -> bool + Send + Sync,
        on_progress: impl Fn(u64) + Send + Sync,
    ) -> Result<u64, ParallelError> {
        let Some(dc_id) = media_dc_id(media) else {
            return Err(ParallelError::Other("media 没有 dc_id".into()));
        };
        let Some(location) = media.to_raw_input_location() else {
            return Err(ParallelError::Other("media 无法下载".into()));
        };
        if total <= start {
            return Ok(total);
        }

        let lanes = self
            .ensure_lanes(client, api_id, proxy_url.as_deref(), dc_id)
            .await?;
        if lanes.len() < 2 {
            return Err(ParallelError::Other(format!(
                "只建了 {} 条下载连接",
                lanes.len()
            )));
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(tmp)
            .map_err(|err| ParallelError::Other(err.to_string()))?;
        file.set_len(start)
            .map_err(|err| ParallelError::Other(err.to_string()))?;

        let next_offset = Arc::new(AtomicU64::new(start));
        let written_at = Arc::new(AtomicU64::new(start));
        let abort = Arc::new(AtomicBool::new(false));
        let window = DOWNLOAD_CHUNK * (lanes.len() as u64 + 2).max(4);
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<(u64, Vec<u8>), ParallelError>>(lanes.len() * 2);
        let mut tasks = JoinSet::new();

        for lane in lanes {
            let next_offset = next_offset.clone();
            let written_at = written_at.clone();
            let abort = abort.clone();
            let tx = tx.clone();
            let location = location.clone();
            let client = client.clone();
            tasks.spawn(async move {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let offset = next_offset.load(Ordering::Relaxed);
                    if offset >= total {
                        break;
                    }
                    let done = written_at.load(Ordering::Relaxed);
                    if offset >= done.saturating_add(window) {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        continue;
                    }
                    if next_offset
                        .compare_exchange(
                            offset,
                            offset.saturating_add(DOWNLOAD_CHUNK),
                            Ordering::SeqCst,
                            Ordering::Relaxed,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    match fetch_chunk(&client, &lane, dc_id, &location, offset).await {
                        Ok(bytes) => {
                            if bytes.is_empty() {
                                break;
                            }
                            let short = (bytes.len() as u64) < DOWNLOAD_CHUNK;
                            if !send_chunk(&tx, &abort, Ok((offset, bytes))).await {
                                break;
                            }
                            if short {
                                break;
                            }
                        }
                        Err(err) => {
                            abort.store(true, Ordering::Relaxed);
                            let _ = send_chunk(&tx, &abort, Err(err)).await;
                            break;
                        }
                    }
                }
            });
        }
        drop(tx);

        let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut written = start;
        let mut result = Ok(total);

        while let Some(item) = rx.recv().await {
            if cancelled() {
                abort.store(true, Ordering::Relaxed);
                result = Err(ParallelError::Cancelled {
                    keep_part: keep_part(),
                });
                break;
            }
            match item {
                Ok((offset, data)) => {
                    pending.insert(offset, data);
                    while let Some(data) = pending.remove(&written) {
                        if let Err(err) = file.write_all(&data) {
                            abort.store(true, Ordering::Relaxed);
                            result = Err(ParallelError::Other(err.to_string()));
                            break;
                        }
                        written += data.len() as u64;
                        written_at.store(written, Ordering::Relaxed);
                        on_progress(written);
                    }
                    if result.is_err() {
                        break;
                    }
                    if written >= total {
                        break;
                    }
                }
                Err(err) => {
                    abort.store(true, Ordering::Relaxed);
                    result = Err(err);
                    break;
                }
            }
        }
        abort.store(true, Ordering::Relaxed);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}

        if result.is_ok() && written < total {
            result = Err(ParallelError::Other(format!(
                "并行下载不完整 {written}/{total}"
            )));
        }
        if result.is_ok() {
            let _ = file.flush();
            Ok(written)
        } else {
            let _ = file.flush();
            result
        }
    }

    async fn ensure_lanes(
        &self,
        client: &Client,
        api_id: i32,
        proxy_url: Option<&str>,
        dc_id: i32,
    ) -> Result<Vec<Arc<Mutex<EncryptedSender>>>, ParallelError> {
        {
            let inner = self.inner.lock().await;
            if inner.dc_id == Some(dc_id) && inner.lanes.len() >= 2 {
                return Ok(inner.lanes.clone());
            }
        }

        let addr = resolve_dc_addr(client, dc_id).await?;
        let mut built = Vec::new();
        for i in 0..MEDIA_LANES {
            match connect_lane(client, api_id, proxy_url, addr, dc_id).await {
                Ok(sender) => built.push(Arc::new(Mutex::new(sender))),
                Err(ParallelError::Flood(secs)) => {
                    log::warn!(
                        "media lane {} flood wait {secs}s, keep {}",
                        i + 1,
                        built.len()
                    );
                    break;
                }
                Err(err) => {
                    log::warn!("media lane {} failed: {}", i + 1, err_text(&err));
                }
            }
        }

        let mut inner = self.inner.lock().await;
        inner.dc_id = Some(dc_id);
        inner.lanes = built.clone();
        log::info!("media pool dc{dc_id} lanes={}", built.len());
        Ok(built)
    }

    pub async fn invalidate(&self) {
        let mut inner = self.inner.lock().await;
        inner.dc_id = None;
        inner.lanes.clear();
    }
}

async fn send_chunk(
    tx: &tokio::sync::mpsc::Sender<Result<(u64, Vec<u8>), ParallelError>>,
    abort: &AtomicBool,
    mut item: Result<(u64, Vec<u8>), ParallelError>,
) -> bool {
    loop {
        if abort.load(Ordering::Relaxed) {
            return false;
        }
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(back)) => {
                item = back;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
}

fn err_text(err: &ParallelError) -> String {
    match err {
        ParallelError::Flood(secs) => format!("flood {secs}s"),
        ParallelError::Cancelled { .. } => "cancelled".into(),
        ParallelError::Other(text) => text.clone(),
    }
}

pub fn media_dc_id(media: &Media) -> Option<i32> {
    match media {
        Media::Document(document) => match document.raw.document.as_ref() {
            Some(tl::enums::Document::Document(doc)) => Some(doc.dc_id),
            _ => None,
        },
        Media::Photo(photo) => match photo.raw.photo.as_ref() {
            Some(tl::enums::Photo::Photo(photo)) => Some(photo.dc_id),
            _ => None,
        },
        Media::Sticker(sticker) => match sticker.document.raw.document.as_ref() {
            Some(tl::enums::Document::Document(doc)) => Some(doc.dc_id),
            _ => None,
        },
        _ => None,
    }
}

async fn resolve_dc_addr(client: &Client, dc_id: i32) -> Result<SocketAddr, ParallelError> {
    let config = client
        .invoke(&tl::functions::help::GetConfig {})
        .await
        .map_err(map_invoke)?;
    let tl::enums::Config::Config(config) = config;
    let addrs: Vec<DcAddr> = config
        .dc_options
        .into_iter()
        .map(|tl::enums::DcOption::Option(option)| DcAddr {
            id: option.id,
            ipv6: option.ipv6,
            media_only: option.media_only,
            cdn: option.cdn,
            tcpo_only: option.tcpo_only,
            ip: option.ip_address,
            port: option.port,
        })
        .collect();
    pick_dc_addr(&addrs, dc_id)
        .ok_or_else(|| ParallelError::Other(format!("找不到 DC{dc_id} 地址")))
}

fn pick_dc_addr(opts: &[DcAddr], dc_id: i32) -> Option<SocketAddr> {
    let mut best: Option<&DcAddr> = None;
    for option in opts {
        if option.id != dc_id || option.cdn || option.tcpo_only || option.ipv6 {
            continue;
        }
        match best {
            None => best = Some(option),
            Some(prev) if option.media_only && !prev.media_only => best = Some(option),
            _ => {}
        }
    }
    let option = best?;
    format!("{}:{}", option.ip, option.port).parse().ok()
}

async fn connect_lane(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    addr: SocketAddr,
    dc_id: i32,
) -> Result<EncryptedSender, ParallelError> {
    let server = match proxy_url {
        Some(proxy) => ServerAddr::Proxied {
            address: addr,
            proxy: proxy.to_string(),
        },
        None => ServerAddr::Tcp { address: addr },
    };
    let mut sender = connect(transport::Full::new(), server.clone())
        .await
        .map_err(map_invoke)?;
    init_connection(&mut sender, api_id).await?;
    import_auth(client, &mut sender, dc_id).await?;
    Ok(sender)
}

async fn init_connection(sender: &mut EncryptedSender, api_id: i32) -> Result<(), ParallelError> {
    let params = grammers_client::sender::ConnectionParams::default();
    let request = tl::functions::InvokeWithLayer {
        layer: tl::LAYER,
        query: tl::functions::InitConnection {
            api_id,
            device_model: format!("{} tgd-dl", params.device_model),
            system_version: params.system_version,
            app_version: params.app_version,
            system_lang_code: params.system_lang_code,
            lang_pack: String::new(),
            lang_code: params.lang_code,
            proxy: None,
            params: None,
            query: tl::functions::help::GetConfig {},
        },
    };
    match sender.invoke(&request).await {
        Ok(_) => Ok(()),
        Err(InvocationError::Transport(transport::Error::BadStatus { status: 404 })) => {
            Err(ParallelError::Other("下载连接 404，auth key 无效".into()))
        }
        Err(err) => Err(map_invoke(err)),
    }
}

async fn import_auth(
    client: &Client,
    sender: &mut EncryptedSender,
    dc_id: i32,
) -> Result<(), ParallelError> {
    let exported = client
        .invoke(&tl::functions::auth::ExportAuthorization { dc_id })
        .await
        .map_err(map_invoke)?;
    let tl::enums::auth::ExportedAuthorization::Authorization(auth) = exported;
    sender
        .invoke(&tl::functions::auth::ImportAuthorization {
            id: auth.id,
            bytes: auth.bytes,
        })
        .await
        .map_err(map_invoke)?;
    Ok(())
}

async fn fetch_chunk(
    client: &Client,
    lane: &Mutex<EncryptedSender>,
    dc_id: i32,
    location: &tl::enums::InputFileLocation,
    offset: u64,
) -> Result<Vec<u8>, ParallelError> {
    let request = tl::functions::upload::GetFile {
        precise: true,
        cdn_supported: false,
        location: location.clone(),
        offset: offset as i64,
        limit: DOWNLOAD_CHUNK as i32,
    };
    let mut sender = lane.lock().await;
    match sender.invoke(&request).await {
        Ok(tl::enums::upload::File::File(file)) => Ok(file.bytes),
        Ok(tl::enums::upload::File::CdnRedirect(_)) => {
            Err(ParallelError::Other("CDN 跳转未实现".into()))
        }
        Err(InvocationError::Rpc(err)) if err.name == "AUTH_KEY_UNREGISTERED" => {
            import_auth(client, &mut sender, dc_id).await?;
            match sender.invoke(&request).await {
                Ok(tl::enums::upload::File::File(file)) => Ok(file.bytes),
                Ok(tl::enums::upload::File::CdnRedirect(_)) => {
                    Err(ParallelError::Other("CDN 跳转未实现".into()))
                }
                Err(err) => Err(map_invoke(err)),
            }
        }
        Err(err) => Err(map_invoke(err)),
    }
}

fn map_invoke(err: InvocationError) -> ParallelError {
    match err {
        InvocationError::Rpc(rpc) if rpc.code == 420 || rpc.name == "FLOOD_WAIT" => {
            ParallelError::Flood(rpc.value.unwrap_or(15) as u64)
        }
        other => ParallelError::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: i32, media_only: bool, ip: &str) -> DcAddr {
        DcAddr {
            id,
            ipv6: false,
            media_only,
            cdn: false,
            tcpo_only: false,
            ip: ip.into(),
            port: 443,
        }
    }

    #[test]
    fn pick_prefers_media_only() {
        let opts = vec![
            addr(2, false, "1.1.1.1"),
            addr(2, true, "2.2.2.2"),
            addr(1, true, "3.3.3.3"),
        ];
        let picked = pick_dc_addr(&opts, 2).unwrap();
        assert_eq!(picked.ip().to_string(), "2.2.2.2");
        assert_eq!(picked.port(), 443);
    }

    #[test]
    fn pick_skips_cdn_and_ipv6() {
        let opts = vec![
            DcAddr {
                id: 4,
                ipv6: true,
                media_only: true,
                cdn: false,
                tcpo_only: false,
                ip: "2001:db8::1".into(),
                port: 443,
            },
            DcAddr {
                id: 4,
                ipv6: false,
                media_only: false,
                cdn: true,
                tcpo_only: false,
                ip: "8.8.8.8".into(),
                port: 443,
            },
            addr(4, false, "9.9.9.9"),
        ];
        assert_eq!(pick_dc_addr(&opts, 4).unwrap().ip().to_string(), "9.9.9.9");
    }

    #[test]
    fn parallel_threshold() {
        assert!(!MediaPool::should_parallel(1024 * 1024));
        assert!(MediaPool::should_parallel(PARALLEL_MIN_SIZE));
    }
}
