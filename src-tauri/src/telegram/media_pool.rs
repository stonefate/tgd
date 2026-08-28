use std::collections::{BTreeMap, HashMap};
use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use grammers_client::media::{Downloadable, Media};
use grammers_client::sender::{connect, ServerAddr};
use grammers_client::tl;
use grammers_client::{Client, InvocationError};
use grammers_mtproto::transport;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::telegram::download::{clear_part_off, write_part_off, DOWNLOAD_CHUNK};

/// 额外 MTProto 连接数上限。每条大约 1~2 MB/s。
pub const MEDIA_LANES: usize = 8;
/// 每个大文件至少独占这么多条额外连接。
pub const MIN_LANES_PER_FILE: usize = 2;
/// 同时走并行分块的大文件上限：8 / 2 = 4。
pub const MAX_PARALLEL_LARGE: usize = MEDIA_LANES / MIN_LANES_PER_FILE;
/// 小于这个的文件走主连接串行，避免握手开销。
pub const PARALLEL_MIN_SIZE: u64 = 4 * 1024 * 1024;
/// 单块 GetFile / 抢锁超时。
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// 额外 lane 建连超时。主连接忙着回爬，下载必须走专用连接。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL: Duration = Duration::from_millis(200);
const FILE_MIGRATE: i32 = 303;
/// `dc == 0` 表示走主连接（home DC）。
const HOME_DC: i32 = 0;
/// 单块非致命失败后最多拉几次（含首次）。
const MAX_CHUNK_TRIES: u8 = 3;
/// `.part.off` 与进度上报节拍。
const OFF_FLUSH_EVERY: Duration = Duration::from_millis(200);

type EncryptedSender =
    grammers_client::sender::Sender<transport::Full, grammers_mtproto::mtp::Encrypted>;

pub enum ParallelError {
    Flood(u64),
    ExpiredRef,
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

#[derive(Clone)]
struct Lane {
    dc_id: i32,
    addr: SocketAddr,
    sender: Arc<Mutex<EncryptedSender>>,
}

struct PoolInner {
    dc_id: Option<i32>,
    free: Vec<Lane>,
}

pub struct MediaPool {
    inner: StdMutex<PoolInner>,
}

/// 未领取分块游标 + 失败回退栈。lane 先拿失败块，再 CAS 新 offset。
struct ChunkQueue {
    next_offset: AtomicU64,
    total: u64,
    retry: StdMutex<Vec<u64>>,
    tries: StdMutex<HashMap<u64, u8>>,
}

impl ChunkQueue {
    fn new(start: u64, total: u64) -> Arc<Self> {
        Arc::new(Self {
            next_offset: AtomicU64::new(start),
            total,
            retry: StdMutex::new(Vec::new()),
            tries: StdMutex::new(HashMap::new()),
        })
    }

    fn take(&self) -> Option<u64> {
        {
            let mut retry = self.retry.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(offset) = retry.pop() {
                return Some(offset);
            }
        }
        loop {
            let offset = self.next_offset.load(Ordering::Relaxed);
            if offset >= self.total {
                let mut retry = self.retry.lock().unwrap_or_else(|err| err.into_inner());
                return retry.pop();
            }
            if self
                .next_offset
                .compare_exchange(
                    offset,
                    offset.saturating_add(DOWNLOAD_CHUNK),
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(offset);
            }
        }
    }

    /// 非致命失败：还能重试返回 `true`，次数用尽返回 `false`。
    fn requeue(&self, offset: u64) -> bool {
        let mut tries = self.tries.lock().unwrap_or_else(|err| err.into_inner());
        let n = tries.entry(offset).or_insert(0);
        *n = n.saturating_add(1);
        if *n >= MAX_CHUNK_TRIES {
            return false;
        }
        drop(tries);
        self.retry
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(offset);
        true
    }
}

struct ProgressFlush {
    last_off: Instant,
    last_progress: Instant,
}

impl ProgressFlush {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_off: now,
            last_progress: now,
        }
    }

    fn maybe(&mut self, tmp: &Path, written: u64, received: u64, on_progress: &impl Fn(u64)) {
        let now = Instant::now();
        if now.duration_since(self.last_off) >= OFF_FLUSH_EVERY {
            write_part_off(tmp, written);
            self.last_off = now;
        }
        if now.duration_since(self.last_progress) >= OFF_FLUSH_EVERY {
            on_progress(received);
            self.last_progress = now;
        }
    }

    fn force(&mut self, tmp: &Path, written: u64, received: u64, on_progress: &impl Fn(u64)) {
        write_part_off(tmp, written);
        on_progress(received);
        let now = Instant::now();
        self.last_off = now;
        self.last_progress = now;
    }
}

impl MediaPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(PoolInner {
                dc_id: None,
                free: Vec::new(),
            }),
        })
    }

    pub fn should_parallel(total: u64) -> bool {
        total >= PARALLEL_MIN_SIZE
    }

    /// 同时在飞的大文件各分到几条额外连接。
    pub fn lanes_per_file(large_in_batch: usize) -> usize {
        let n = large_in_batch.max(1).min(MAX_PARALLEL_LARGE);
        (MEDIA_LANES / n).max(MIN_LANES_PER_FILE)
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
        lane_budget: usize,
        cancelled: impl Fn() -> bool + Send + Sync,
        keep_part: impl Fn() -> bool + Send + Sync,
        on_progress: impl Fn(u64) + Send + Sync,
    ) -> Result<u64, ParallelError> {
        let Some(location) = media.to_raw_input_location() else {
            return Err(ParallelError::Other("media 无法下载".into()));
        };
        if total <= start {
            return Ok(total);
        }
        let media_dc = media_dc_id(media).unwrap_or(HOME_DC);
        let extra_n = if media_dc == HOME_DC {
            0
        } else {
            lane_budget.clamp(MIN_LANES_PER_FILE, MEDIA_LANES)
        };

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(tmp)
            .map_err(|err| ParallelError::Other(err.to_string()))?;
        write_part_off(tmp, start);
        file.set_len(total)
            .map_err(|err| ParallelError::Other(err.to_string()))?;

        let chunks = ChunkQueue::new(start, total);
        let abort = Arc::new(AtomicBool::new(false));
        let pool_dc = Arc::new(AtomicI32::new(HOME_DC));
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<(u64, Vec<u8>), ParallelError>>(MEDIA_LANES * 2);
        let mut chunk_tx = Some(tx);
        let mut tasks = JoinSet::new();
        on_progress(start);
        let mut flush = ProgressFlush::new();

        let cached = self.take_free(media_dc, extra_n);
        let need = extra_n.saturating_sub(cached.len());
        let mut leased = cached.clone();
        let mut extra_workers = 0usize;
        let mut pool_started = false;
        let (lane_tx, mut lane_rx) = tokio::sync::mpsc::channel::<Lane>(MEDIA_LANES);
        for lane in cached {
            let tx = chunk_tx.as_ref().unwrap().clone();
            spawn_lane_worker(
                &mut tasks,
                client.clone(),
                api_id,
                proxy_url.clone(),
                lane,
                location.clone(),
                chunks.clone(),
                abort.clone(),
                tx,
            );
            extra_workers += 1;
        }
        if need > 0 {
            let client = client.clone();
            let proxy_url = proxy_url.clone();
            let abort_connect = abort.clone();
            tasks.spawn(async move {
                connect_extras(
                    &client,
                    api_id,
                    proxy_url.as_deref(),
                    media_dc,
                    need,
                    &abort_connect,
                    lane_tx,
                )
                .await;
            });
        } else {
            drop(lane_tx);
        }
        if extra_n == 0 {
            spawn_pool_worker(
                &mut tasks,
                client.clone(),
                pool_dc.clone(),
                location.clone(),
                chunks.clone(),
                abort.clone(),
                chunk_tx.as_ref().unwrap().clone(),
            );
            pool_started = true;
            chunk_tx.take();
        } else if need == 0 {
            chunk_tx.take();
        }

        let mut pending: BTreeMap<u64, u64> = BTreeMap::new();
        let mut written = start;
        let mut received = start;
        let mut result = Ok(total);
        let mut extras_open = need > 0;

        loop {
            tokio::select! {
                lane = lane_rx.recv(), if extras_open => {
                    match lane {
                        Some(lane) => {
                            let Some(tx) = chunk_tx.as_ref() else {
                                continue;
                            };
                            leased.push(lane.clone());
                            spawn_lane_worker(
                                &mut tasks,
                                client.clone(),
                                api_id,
                                proxy_url.clone(),
                                lane,
                                location.clone(),
                                chunks.clone(),
                                abort.clone(),
                                tx.clone(),
                            );
                            extra_workers += 1;
                        }
                        None => {
                            extras_open = false;
                            if extra_workers == 0 && !pool_started {
                                if let Some(tx) = chunk_tx.as_ref() {
                                    log::warn!("media extra lanes failed, fallback to main connection");
                                    spawn_pool_worker(
                                        &mut tasks,
                                        client.clone(),
                                        pool_dc.clone(),
                                        location.clone(),
                                        chunks.clone(),
                                        abort.clone(),
                                        tx.clone(),
                                    );
                                    pool_started = true;
                                }
                            }
                            chunk_tx.take();
                        }
                    }
                }
                item = rx.recv() => {
                    let Some(item) = item else {
                        break;
                    };
                    if cancelled() {
                        abort.store(true, Ordering::Relaxed);
                        result = Err(ParallelError::Cancelled {
                            keep_part: keep_part(),
                        });
                        break;
                    }
                    match item {
                        Ok((offset, data)) => {
                            let len = data.len() as u64;
                            if let Err(err) = file
                                .seek(SeekFrom::Start(offset))
                                .and_then(|_| file.write_all(&data))
                            {
                                abort.store(true, Ordering::Relaxed);
                                result = Err(ParallelError::Other(err.to_string()));
                                break;
                            }
                            received = received.saturating_add(len).min(total);
                            pending.insert(offset, len);
                            while let Some(len) = pending.remove(&written) {
                                written += len;
                            }
                            flush.maybe(tmp, written, received, &on_progress);
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
                _ = tokio::time::sleep(CANCEL_POLL) => {
                    if cancelled() {
                        abort.store(true, Ordering::Relaxed);
                        result = Err(ParallelError::Cancelled {
                            keep_part: keep_part(),
                        });
                        break;
                    }
                }
            }
        }
        flush.force(tmp, written, received, &on_progress);
        drop(chunk_tx);
        abort.store(true, Ordering::Relaxed);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.put_free(leased);

        if result.is_ok() && written < total {
            result = Err(ParallelError::Other(format!(
                "并行下载不完整 {written}/{total}"
            )));
        }
        let _ = file.flush();
        if result.is_ok() {
            clear_part_off(tmp);
            Ok(written)
        } else if file.set_len(written).is_ok() {
            let _ = file.flush();
            clear_part_off(tmp);
            result
        } else {
            result
        }
    }

    pub async fn invalidate(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.dc_id = None;
        inner.free.clear();
    }

    fn take_free(&self, dc_id: i32, n: usize) -> Vec<Lane> {
        if n == 0 || dc_id == HOME_DC {
            return Vec::new();
        }
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if inner.dc_id != Some(dc_id) {
            inner.free.clear();
            inner.dc_id = Some(dc_id);
            return Vec::new();
        }
        let take = n.min(inner.free.len());
        inner.free.drain(..take).collect()
    }

    fn put_free(&self, lanes: Vec<Lane>) {
        if lanes.is_empty() {
            return;
        }
        let dc_id = lanes[0].dc_id;
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if inner.dc_id != Some(dc_id) {
            inner.free.clear();
            inner.dc_id = Some(dc_id);
        }
        inner.free.extend(lanes);
        if inner.free.len() > MEDIA_LANES {
            inner.free.truncate(MEDIA_LANES);
        }
    }
}

fn spawn_pool_worker(
    tasks: &mut JoinSet<()>,
    client: Client,
    pool_dc: Arc<AtomicI32>,
    location: tl::enums::InputFileLocation,
    chunks: Arc<ChunkQueue>,
    abort: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<Result<(u64, Vec<u8>), ParallelError>>,
) {
    tasks.spawn(async move {
        run_offset_loop(chunks, abort, tx, move |offset| {
            let client = client.clone();
            let pool_dc = pool_dc.clone();
            let location = location.clone();
            async move { fetch_via_pool(&client, &pool_dc, &location, offset).await }
        })
        .await;
    });
}

fn spawn_lane_worker(
    tasks: &mut JoinSet<()>,
    client: Client,
    api_id: i32,
    proxy_url: Option<String>,
    lane: Lane,
    location: tl::enums::InputFileLocation,
    chunks: Arc<ChunkQueue>,
    abort: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<Result<(u64, Vec<u8>), ParallelError>>,
) {
    tasks.spawn(async move {
        let addr = lane.addr;
        run_offset_loop(chunks, abort, tx, move |offset| {
            let client = client.clone();
            let lane = lane.clone();
            let location = location.clone();
            let proxy_url = proxy_url.clone();
            async move {
                fetch_via_lane(
                    &client,
                    api_id,
                    proxy_url.as_deref(),
                    addr,
                    &lane,
                    &location,
                    offset,
                )
                .await
            }
        })
        .await;
    });
}

async fn run_offset_loop<F, Fut>(
    chunks: Arc<ChunkQueue>,
    abort: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<Result<(u64, Vec<u8>), ParallelError>>,
    fetch: F,
) where
    F: Fn(u64) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, ParallelError>>,
{
    loop {
        if abort.load(Ordering::Relaxed) {
            break;
        }
        let Some(offset) = chunks.take() else {
            break;
        };
        match fetch(offset).await {
            Ok(bytes) => {
                if bytes.is_empty() {
                    if chunks.requeue(offset) {
                        log::warn!("download chunk {offset} empty, retry");
                        continue;
                    }
                    let _ = send_chunk(
                        &tx,
                        &abort,
                        Err(ParallelError::Other(format!("分块为空 {offset}"))),
                    )
                    .await;
                    abort.store(true, Ordering::Relaxed);
                    break;
                }
                if !send_chunk(&tx, &abort, Ok((offset, bytes))).await {
                    break;
                }
            }
            Err(err) => {
                if is_fatal(&err) {
                    let _ = send_chunk(&tx, &abort, Err(err)).await;
                    abort.store(true, Ordering::Relaxed);
                    break;
                }
                if chunks.requeue(offset) {
                    log::warn!("download chunk {offset} retry: {}", err_text(&err));
                    continue;
                }
                log::warn!(
                    "download chunk {offset} retries exhausted: {}",
                    err_text(&err)
                );
                let _ = send_chunk(&tx, &abort, Err(err)).await;
                abort.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
}

async fn connect_extras(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    dc_id: i32,
    n: usize,
    abort: &AtomicBool,
    lane_tx: tokio::sync::mpsc::Sender<Lane>,
) {
    if dc_id == HOME_DC || n == 0 {
        return;
    }
    let addrs = match tokio::time::timeout(CONNECT_TIMEOUT, resolve_dc_addrs(client, dc_id)).await {
        Ok(Ok(addrs)) if !addrs.is_empty() => addrs,
        Ok(Ok(_)) => {
            log::warn!("media extra dc{dc_id} has no addr");
            return;
        }
        Ok(Err(err)) => {
            log::warn!("media extra dc{dc_id} addr: {}", err_text(&err));
            return;
        }
        Err(_) => {
            log::warn!("media extra dc{dc_id} addr timeout");
            return;
        }
    };
    let mut preferred = None;
    let mut got = 0usize;
    for i in 0..n {
        if abort.load(Ordering::Relaxed) {
            break;
        }
        let result = if i == 0 {
            connect_lane_race(client, api_id, proxy_url, dc_id, &addrs).await
        } else {
            let addr = preferred.unwrap_or(addrs[0]);
            connect_lane_timed(client, api_id, proxy_url, addr, dc_id).await
        };
        match result {
            Ok(lane) => {
                preferred = Some(lane.addr);
                got += 1;
                if lane_tx.send(lane).await.is_err() {
                    break;
                }
            }
            Err(ParallelError::Flood(secs)) => {
                log::warn!("media extra flood wait {secs}s, keep {got}");
                break;
            }
            Err(err) => {
                log::warn!("media extra failed: {}", err_text(&err));
                if i == 0 {
                    break;
                }
            }
        }
    }
    if got > 0 {
        log::info!("media extra dc{dc_id} lanes={got}");
    }
}

async fn connect_lane_timed(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    addr: SocketAddr,
    dc_id: i32,
) -> Result<Lane, ParallelError> {
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_lane(client, api_id, proxy_url, addr, dc_id),
    )
    .await
    {
        Ok(Ok(sender)) => Ok(Lane {
            dc_id,
            addr,
            sender: Arc::new(Mutex::new(sender)),
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(ParallelError::Other("下载连接超时".into())),
    }
}

/// 第一条并行试所有 DC 地址，谁先通则用谁（media_only 通常更快）。
async fn connect_lane_race(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    dc_id: i32,
    addrs: &[SocketAddr],
) -> Result<Lane, ParallelError> {
    if addrs.len() == 1 {
        return connect_lane_timed(client, api_id, proxy_url, addrs[0], dc_id).await;
    }
    let mut set = JoinSet::new();
    for &addr in addrs {
        let client = client.clone();
        let proxy = proxy_url.map(str::to_string);
        set.spawn(async move {
            let result = connect_lane_timed(&client, api_id, proxy.as_deref(), addr, dc_id).await;
            (addr, result)
        });
    }
    let mut last_err = ParallelError::Other("找不到下载连接".into());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((_, Ok(lane))) => {
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Ok(lane);
            }
            Ok((_, Err(ParallelError::Flood(secs)))) => {
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Err(ParallelError::Flood(secs));
            }
            Ok((_, Err(err))) => last_err = err,
            Err(err) => last_err = ParallelError::Other(err.to_string()),
        }
    }
    Err(last_err)
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
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
}

fn err_text(err: &ParallelError) -> String {
    match err {
        ParallelError::Flood(secs) => format!("flood {secs}s"),
        ParallelError::ExpiredRef => "FILE_REFERENCE_EXPIRED".into(),
        ParallelError::Cancelled { .. } => "cancelled".into(),
        ParallelError::Other(text) => text.clone(),
    }
}

fn is_lane_dead(err: &ParallelError) -> bool {
    match err {
        ParallelError::Other(text) => {
            let text = text.to_ascii_lowercase();
            text.contains("超时")
                || text.contains("连接断开")
                || text.contains("read 0 bytes")
                || text.contains("connection reset")
                || text.contains("broken pipe")
        }
        _ => false,
    }
}

fn is_fatal(err: &ParallelError) -> bool {
    matches!(
        err,
        ParallelError::Flood(_) | ParallelError::ExpiredRef | ParallelError::Cancelled { .. }
    )
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

async fn resolve_dc_addrs(client: &Client, dc_id: i32) -> Result<Vec<SocketAddr>, ParallelError> {
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
    let picked = dc_addrs(&addrs, dc_id);
    if picked.is_empty() {
        Err(ParallelError::Other(format!("找不到 DC{dc_id} 地址")))
    } else {
        Ok(picked)
    }
}

/// media_only 优先（带宽更高），普通 DC 地址作备选。
fn dc_addrs(opts: &[DcAddr], dc_id: i32) -> Vec<SocketAddr> {
    let mut regular = Vec::new();
    let mut media = Vec::new();
    for option in opts {
        if option.id != dc_id || option.cdn || option.tcpo_only || option.ipv6 {
            continue;
        }
        let Ok(addr) = format!("{}:{}", option.ip, option.port).parse() else {
            continue;
        };
        if option.media_only {
            media.push(addr);
        } else {
            regular.push(addr);
        }
    }
    media.extend(regular);
    media
}

fn pick_dc_addr(opts: &[DcAddr], dc_id: i32) -> Option<SocketAddr> {
    dc_addrs(opts, dc_id).into_iter().next()
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
    import_auth_lane(client, &mut sender, dc_id).await?;
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

async fn import_auth_lane(
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

async fn import_auth_pool(client: &Client, dc_id: i32) -> Result<(), ParallelError> {
    let exported = client
        .invoke(&tl::functions::auth::ExportAuthorization { dc_id })
        .await
        .map_err(map_invoke)?;
    let tl::enums::auth::ExportedAuthorization::Authorization(auth) = exported;
    client
        .invoke_in_dc(
            dc_id,
            &tl::functions::auth::ImportAuthorization {
                id: auth.id,
                bytes: auth.bytes,
            },
        )
        .await
        .map_err(map_invoke)?;
    Ok(())
}

async fn lock_lane(
    lane: &Lane,
) -> Result<tokio::sync::MutexGuard<'_, EncryptedSender>, ParallelError> {
    tokio::time::timeout(CHUNK_TIMEOUT, lane.sender.lock())
        .await
        .map_err(|_| ParallelError::Other("下载连接占用超时".into()))
}

fn get_file_request(
    location: &tl::enums::InputFileLocation,
    offset: u64,
) -> tl::functions::upload::GetFile {
    tl::functions::upload::GetFile {
        precise: true,
        cdn_supported: false,
        location: location.clone(),
        offset: offset as i64,
        limit: DOWNLOAD_CHUNK as i32,
    }
}

async fn fetch_via_pool(
    client: &Client,
    pool_dc: &AtomicI32,
    location: &tl::enums::InputFileLocation,
    offset: u64,
) -> Result<Vec<u8>, ParallelError> {
    let request = get_file_request(location, offset);
    let mut dc = pool_dc.load(Ordering::Relaxed);
    for _ in 0..3 {
        match invoke_get_file(client, dc, &request).await {
            Ok(bytes) => return Ok(bytes),
            Err(GetFileError::AuthUnregistered) => {
                let target = if dc == HOME_DC {
                    return Err(ParallelError::Other("AUTH_KEY_UNREGISTERED".into()));
                } else {
                    dc
                };
                import_auth_pool(client, target).await?;
            }
            Err(GetFileError::Migrate(next)) => {
                pool_dc.store(next, Ordering::Relaxed);
                dc = next;
            }
            Err(GetFileError::Fail(err)) if is_lane_dead(&err) => {
                return Err(err);
            }
            Err(GetFileError::Fail(err)) => return Err(err),
        }
    }
    Err(ParallelError::Other("下载超时".into()))
}

async fn invoke_get_file(
    client: &Client,
    dc: i32,
    request: &tl::functions::upload::GetFile,
) -> Result<Vec<u8>, GetFileError> {
    let result = if dc == HOME_DC {
        tokio::time::timeout(CHUNK_TIMEOUT, client.invoke(request)).await
    } else {
        tokio::time::timeout(CHUNK_TIMEOUT, client.invoke_in_dc(dc, request)).await
    };
    match result {
        Ok(Ok(tl::enums::upload::File::File(file))) => Ok(file.bytes),
        Ok(Ok(tl::enums::upload::File::CdnRedirect(_))) => Err(GetFileError::Fail(
            ParallelError::Other("CDN 跳转未实现".into()),
        )),
        Ok(Err(InvocationError::Rpc(err))) if err.name == "AUTH_KEY_UNREGISTERED" => {
            Err(GetFileError::AuthUnregistered)
        }
        Ok(Err(InvocationError::Rpc(err))) if err.code == FILE_MIGRATE => {
            Err(GetFileError::Migrate(err.value.unwrap_or(0) as i32))
        }
        Ok(Err(err)) => Err(GetFileError::Fail(map_invoke(err))),
        Err(_) => Err(GetFileError::Fail(ParallelError::Other("下载超时".into()))),
    }
}

async fn fetch_via_lane(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    addr: SocketAddr,
    lane: &Lane,
    location: &tl::enums::InputFileLocation,
    offset: u64,
) -> Result<Vec<u8>, ParallelError> {
    let request = get_file_request(location, offset);
    let mut sender = lock_lane(lane).await?;
    match timed_get_file(&mut sender, &request).await {
        Ok(bytes) => Ok(bytes),
        Err(GetFileError::AuthUnregistered) => {
            import_auth_lane(client, &mut sender, lane.dc_id).await?;
            match timed_get_file(&mut sender, &request).await {
                Ok(bytes) => Ok(bytes),
                Err(GetFileError::Fail(err)) if is_lane_dead(&err) => {
                    revive_and_retry(
                        client,
                        api_id,
                        proxy_url,
                        addr,
                        &mut sender,
                        lane.dc_id,
                        &request,
                    )
                    .await
                }
                Err(GetFileError::AuthUnregistered) => {
                    revive_and_retry(
                        client,
                        api_id,
                        proxy_url,
                        addr,
                        &mut sender,
                        lane.dc_id,
                        &request,
                    )
                    .await
                }
                Err(GetFileError::Migrate(next)) => {
                    Err(ParallelError::Other(format!("FILE_MIGRATE_{next}")))
                }
                Err(GetFileError::Fail(err)) => Err(err),
            }
        }
        Err(GetFileError::Fail(err)) if is_lane_dead(&err) => {
            revive_and_retry(
                client,
                api_id,
                proxy_url,
                addr,
                &mut sender,
                lane.dc_id,
                &request,
            )
            .await
        }
        Err(GetFileError::Migrate(next)) => {
            Err(ParallelError::Other(format!("FILE_MIGRATE_{next}")))
        }
        Err(GetFileError::Fail(err)) => Err(err),
    }
}

async fn revive_and_retry(
    client: &Client,
    api_id: i32,
    proxy_url: Option<&str>,
    addr: SocketAddr,
    sender: &mut EncryptedSender,
    dc_id: i32,
    request: &tl::functions::upload::GetFile,
) -> Result<Vec<u8>, ParallelError> {
    log::warn!("media lane dc{dc_id} dead, reconnect");
    *sender = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_lane(client, api_id, proxy_url, addr, dc_id),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(ParallelError::Other("下载连接超时".into())),
    };
    match timed_get_file(sender, request).await {
        Ok(bytes) => Ok(bytes),
        Err(GetFileError::AuthUnregistered) => {
            import_auth_lane(client, sender, dc_id).await?;
            match timed_get_file(sender, request).await {
                Ok(bytes) => Ok(bytes),
                Err(GetFileError::Fail(err)) => Err(err),
                Err(GetFileError::AuthUnregistered) => {
                    Err(ParallelError::Other("AUTH_KEY_UNREGISTERED".into()))
                }
                Err(GetFileError::Migrate(next)) => {
                    Err(ParallelError::Other(format!("FILE_MIGRATE_{next}")))
                }
            }
        }
        Err(GetFileError::Fail(err)) => Err(err),
        Err(GetFileError::Migrate(next)) => {
            Err(ParallelError::Other(format!("FILE_MIGRATE_{next}")))
        }
    }
}

enum GetFileError {
    AuthUnregistered,
    Migrate(i32),
    Fail(ParallelError),
}

async fn timed_get_file(
    sender: &mut EncryptedSender,
    request: &tl::functions::upload::GetFile,
) -> Result<Vec<u8>, GetFileError> {
    match tokio::time::timeout(CHUNK_TIMEOUT, sender.invoke(request)).await {
        Ok(Ok(tl::enums::upload::File::File(file))) => Ok(file.bytes),
        Ok(Ok(tl::enums::upload::File::CdnRedirect(_))) => Err(GetFileError::Fail(
            ParallelError::Other("CDN 跳转未实现".into()),
        )),
        Ok(Err(InvocationError::Rpc(err))) if err.name == "AUTH_KEY_UNREGISTERED" => {
            Err(GetFileError::AuthUnregistered)
        }
        Ok(Err(InvocationError::Rpc(err))) if err.code == FILE_MIGRATE => {
            Err(GetFileError::Migrate(err.value.unwrap_or(0) as i32))
        }
        Ok(Err(err)) => Err(GetFileError::Fail(map_invoke(err))),
        Err(_) => Err(GetFileError::Fail(ParallelError::Other("下载超时".into()))),
    }
}

fn map_invoke(err: InvocationError) -> ParallelError {
    match err {
        InvocationError::Rpc(rpc) if rpc.code == 420 || rpc.name == "FLOOD_WAIT" => {
            ParallelError::Flood(rpc.value.unwrap_or(15) as u64)
        }
        InvocationError::Rpc(rpc) if rpc.name.starts_with("FILE_REFERENCE") => {
            ParallelError::ExpiredRef
        }
        InvocationError::Io(_) => ParallelError::Other("连接断开".into()),
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
    fn pick_media_only_before_regular() {
        let opts = vec![
            addr(2, false, "1.1.1.1"),
            addr(2, true, "2.2.2.2"),
            addr(1, true, "3.3.3.3"),
        ];
        let picked = dc_addrs(&opts, 2);
        assert_eq!(picked[0].ip().to_string(), "2.2.2.2");
        assert_eq!(picked[1].ip().to_string(), "1.1.1.1");
        assert_eq!(pick_dc_addr(&opts, 2).unwrap().ip().to_string(), "2.2.2.2");
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

    #[test]
    fn lanes_split_among_files() {
        assert_eq!(MediaPool::lanes_per_file(1), 8);
        assert_eq!(MediaPool::lanes_per_file(2), 4);
        assert_eq!(MediaPool::lanes_per_file(3), 2);
        assert_eq!(MediaPool::lanes_per_file(4), 2);
        assert_eq!(MediaPool::lanes_per_file(8), 2);
        assert_eq!(MediaPool::lanes_per_file(0), 8);
        assert!(MediaPool::lanes_per_file(4) * MAX_PARALLEL_LARGE <= MEDIA_LANES);
    }

    #[test]
    fn lane_dead_matches_disconnect() {
        assert!(is_lane_dead(&ParallelError::Other("连接断开".into())));
        assert!(is_lane_dead(&ParallelError::Other("下载超时".into())));
        assert!(is_lane_dead(&ParallelError::Other(
            "下载连接占用超时".into()
        )));
        assert!(is_lane_dead(&ParallelError::Other(
            "request error: read 0 bytes".into()
        )));
        assert!(!is_lane_dead(&ParallelError::Flood(5)));
        assert!(!is_lane_dead(&ParallelError::ExpiredRef));
        assert!(!is_fatal(&ParallelError::Other("连接断开".into())));
        assert!(is_fatal(&ParallelError::ExpiredRef));
    }

    #[test]
    fn chunk_queue_retries_before_new_offsets() {
        let q = ChunkQueue::new(0, DOWNLOAD_CHUNK * 10);
        let first = q.take().unwrap();
        let second = q.take().unwrap();
        assert_eq!(first, 0);
        assert_eq!(second, DOWNLOAD_CHUNK);
        assert!(q.requeue(first));
        assert_eq!(q.take().unwrap(), first);
        assert_eq!(q.take().unwrap(), DOWNLOAD_CHUNK * 2);
    }

    #[test]
    fn chunk_queue_picks_retry_after_all_claimed() {
        let q = ChunkQueue::new(0, DOWNLOAD_CHUNK);
        let offset = q.take().unwrap();
        assert!(q.take().is_none());
        assert!(q.requeue(offset));
        assert_eq!(q.take().unwrap(), offset);
        assert!(q.take().is_none());
    }

    #[test]
    fn chunk_queue_exhausts_after_max_tries() {
        let q = ChunkQueue::new(0, DOWNLOAD_CHUNK * 2);
        let offset = q.take().unwrap();
        for _ in 0..(MAX_CHUNK_TRIES - 1) {
            assert!(q.requeue(offset));
            assert_eq!(q.take().unwrap(), offset);
        }
        assert!(!q.requeue(offset));
        assert_eq!(q.take().unwrap(), DOWNLOAD_CHUNK);
    }

    #[tokio::test]
    async fn offset_loop_requeues_timeout_for_other_worker() {
        let total = DOWNLOAD_CHUNK * 3;
        let chunks = ChunkQueue::new(0, total);
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let fails = Arc::new(AtomicU64::new(0));
        for _ in 0..2 {
            let chunks = chunks.clone();
            let abort = abort.clone();
            let tx = tx.clone();
            let fails = fails.clone();
            tokio::spawn(async move {
                run_offset_loop(chunks, abort, tx, move |offset| {
                    let fails = fails.clone();
                    async move {
                        if offset == 0 && fails.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Err(ParallelError::Other("下载超时".into()));
                        }
                        Ok(vec![1u8; DOWNLOAD_CHUNK as usize])
                    }
                })
                .await;
            });
        }
        drop(tx);
        let mut got = Vec::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok((offset, _)) => got.push(offset),
                Err(err) => panic!("unexpected {}", err_text(&err)),
            }
        }
        got.sort();
        assert_eq!(got, vec![0, DOWNLOAD_CHUNK, DOWNLOAD_CHUNK * 2]);
        assert!(fails.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn offset_loop_fatal_does_not_requeue() {
        let chunks = ChunkQueue::new(0, DOWNLOAD_CHUNK * 4);
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            run_offset_loop(chunks, abort, tx, |_| async {
                Err(ParallelError::ExpiredRef)
            })
            .await;
        });
        match rx.recv().await {
            Some(Err(ParallelError::ExpiredRef)) => {}
            other => panic!(
                "expected expired ref, got {}",
                match other {
                    Some(Ok((offset, _))) => format!("ok {offset}"),
                    Some(Err(err)) => err_text(&err),
                    None => "closed".into(),
                }
            ),
        }
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn offset_loop_exhausts_chunk_retries() {
        let chunks = ChunkQueue::new(0, DOWNLOAD_CHUNK);
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            run_offset_loop(chunks, abort, tx, |_| async {
                Err(ParallelError::Other("下载超时".into()))
            })
            .await;
        });
        match rx.recv().await {
            Some(Err(ParallelError::Other(text))) => {
                assert!(text.contains("超时"), "{text}");
            }
            other => panic!(
                "expected timeout, got {}",
                match other {
                    Some(Ok((offset, _))) => format!("ok {offset}"),
                    Some(Err(err)) => err_text(&err),
                    None => "closed".into(),
                }
            ),
        }
        assert!(rx.recv().await.is_none());
    }
}
