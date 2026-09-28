use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRef, Query, Request, State};
use axum::http::header::{HeaderValue, CACHE_CONTROL};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde::Serialize;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::commands::{
    AppInfo, DownloadItem, DownloadUsage, LogEntry, MessagePage, SearchCursor, TelegramStatus,
};
use crate::error::AppError;
use crate::ilink::IlinkStatus;
use crate::runtime::AppCtx;
use crate::service;
use crate::settings::{ChatDownloadTypes, ProxyConfig};
use crate::telegram::{load_dotenv, ChatItem, DownloadProgress};

mod web {
    use axum::body::Body;
    use axum::http::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
    use axum::http::{StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "../build"]
    struct WebAsset;

    pub fn file(path: &str) -> Option<Response> {
        let file = WebAsset::get(path)?;
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let mut builder = Response::builder().header(
            CONTENT_TYPE,
            HeaderValue::from_str(mime.as_ref())
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        );
        if path.starts_with("_app/") {
            builder = builder.header(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
        }
        Some(
            builder
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        )
    }

    pub fn index() -> Response {
        match file("index.html") {
            Some(res) => res,
            None => (StatusCode::INTERNAL_SERVER_ERROR, "missing web ui").into_response(),
        }
    }

    pub async fn static_or_spa(uri: Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        if path.is_empty() {
            return index();
        }
        if let Some(res) = file(path) {
            return res;
        }
        index()
    }
}

#[derive(Clone)]
struct ServerState {
    ctx: AppCtx,
    web_dir: Option<PathBuf>,
}

impl FromRef<ServerState> for AppCtx {
    fn from_ref(state: &ServerState) -> Self {
        state.ctx.clone()
    }
}

#[derive(Serialize)]
#[serde(tag = "status")]
enum ApiResult<T> {
    #[serde(rename = "ok")]
    Ok { data: T },
    #[serde(rename = "error")]
    Error { error: AppError },
}

impl<T: Serialize> ApiResult<T> {
    fn from_result(result: Result<T, AppError>) -> Json<Self> {
        Json(match result {
            Ok(data) => Self::Ok { data },
            Err(error) => Self::Error { error },
        })
    }
}

async fn wrap<T: Serialize>(
    fut: impl std::future::Future<Output = Result<T, AppError>>,
) -> Json<ApiResult<T>> {
    ApiResult::from_result(fut.await)
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn listen_addr() -> SocketAddr {
    let raw = std::env::var("TGD_LISTEN").unwrap_or_else(|_| "0.0.0.0:8787".into());
    raw.parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8787)))
}

fn web_base() -> String {
    std::env::var("TGD_WEB_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| value.starts_with('/') && value.len() > 1)
        .unwrap_or_default()
}

fn listen_socket() -> Option<PathBuf> {
    std::env::var("TGD_LISTEN_SOCKET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn web_dir_override() -> Option<PathBuf> {
    std::env::var("TGD_WEB_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

pub fn run() {
    load_dotenv();
    crate::app_log::init_env();

    let data_dir = env_path("TGD_DATA_DIR", "/data");
    let download_dir = env_path("TGD_DOWNLOAD_DIR", "/downloads");
    let web_dir = web_dir_override();
    let ctx = AppCtx::new(data_dir, Some(download_dir));
    if let Err(err) = ctx.paths().ensure_dirs() {
        log::warn!("failed to create data dirs: {err}");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let ctx_bg = ctx.clone();
        tokio::spawn(async move {
            if let Err(err) = service::connect_telegram(&ctx_bg, false).await {
                log::warn!("startup connect: {err}");
            }
        });
        crate::ilink::spawn_ilink_worker(ctx.clone());
        let app = router(ServerState { ctx, web_dir });
        serve(app).await;
    });
}

async fn serve(app: Router) {
    let addr = listen_addr();
    let tcp_app = app.clone();
    let tcp = async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|err| panic!("bind {addr}: {err}"));
        log::info!("tgd-server listening on {addr}");
        axum::serve(listener, tcp_app).await
    };

    #[cfg(unix)]
    if let Some(path) = listen_socket() {
        let unix = serve_unix(path, app);
        tokio::select! {
            result = tcp => result.expect("tcp server"),
            result = unix => result.expect("unix server"),
        }
        return;
    }

    tcp.await.expect("tcp server");
}

#[cfg(unix)]
async fn serve_unix(path: PathBuf, app: Router) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
    log::info!("tgd-server listening on unix {}", path.display());
    axum::serve(listener, app).await
}

fn router(state: ServerState) -> Router {
    let api = Router::new()
        .route("/app-info", get(app_info))
        .route("/logs", get(recent_logs))
        .route("/telegram/status", get(telegram_status))
        .route("/telegram/connect", post(connect_telegram))
        .route("/telegram/request-code", post(request_login_code))
        .route("/telegram/submit-code", post(submit_login_code))
        .route("/telegram/submit-password", post(submit_password))
        .route("/telegram/logout", post(logout))
        .route("/chats", get(list_chats))
        .route("/chats/watched", post(set_chat_watched))
        .route("/settings/guest-watch", post(set_guest_watch))
        .route("/settings/guest-watch/add", post(add_guest_watch))
        .route("/settings/guest-watch/remove", post(remove_guest_watch))
        .route(
            "/settings/guest-watch/toggle",
            post(set_guest_watch_enabled),
        )
        .route("/chats/types", post(set_chat_download_types))
        .route("/chats/backfill-days", post(set_chat_backfill_days))
        .route("/chats/alias", post(set_chat_alias))
        .route("/chats/clear-messages", post(clear_chat_messages))
        .route("/chats/check-media", post(check_chat_media))
        .route("/settings/backfill-days", post(set_backfill_days))
        .route("/settings/show-media", post(set_show_media))
        .route("/settings/proxy", post(set_proxy))
        .route("/settings/download-dir", post(set_download_dir))
        .route("/settings/download-dirs", get(download_dir_options))
        .route(
            "/settings/download-concurrency",
            post(set_download_concurrency),
        )
        .route("/settings/min-media-mb", post(set_min_media_mb))
        .route("/settings/download-paused", post(set_download_paused))
        .route("/ilink/status", get(ilink_status))
        .route("/ilink/login", post(ilink_start_login))
        .route("/ilink/logout", post(ilink_logout))
        .route("/ilink/enabled", post(set_ilink_notify_enabled))
        .route("/ilink/test", post(ilink_send_test))
        .route("/settings/autostart", post(set_autostart_noop))
        .route("/downloads/status", get(download_status))
        .route("/downloads", get(list_downloads))
        .route("/downloads/usage", get(download_usage))
        .route("/downloads/cancel", post(cancel_download))
        .route("/messages", get(list_messages))
        .route("/messages/redownload", post(redownload_message_media))
        .route("/search", get(search_messages))
        .route("/open-url", post(open_url))
        .route("/events", get(events))
        .route("/media", get(media))
        .route("/media/thumb", get(media_thumb));

    let mut inner = Router::new().nest("/api", api);
    if let Some(web_dir) = &state.web_dir {
        let assets = web_dir.join("_app");
        let robots = web_dir.join("robots.txt");
        let hashed = Router::new().fallback_service(ServeDir::new(assets)).layer(
            SetResponseHeaderLayer::overriding(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        );
        inner = inner.nest("/_app", hashed);
        if robots.is_file() {
            inner = inner.route_service("/robots.txt", ServeFile::new(robots));
        }
        inner = inner.fallback(get(spa_index));
    } else {
        inner = inner.fallback(get(web::static_or_spa));
    }
    let app = inner
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let base = web_base();
    if base.is_empty() {
        return app;
    }
    Router::new()
        .nest(&base, app)
        .fallback(get(redirect_to_base))
}

async fn redirect_to_base(req: Request) -> Redirect {
    let base = web_base();
    let path = req.uri().path();
    let query = req
        .uri()
        .query()
        .map(|value| format!("?{value}"))
        .unwrap_or_default();
    let rest = if path == "/" { "/" } else { path };
    Redirect::temporary(&format!("{base}{rest}{query}"))
}

async fn spa_index(State(state): State<ServerState>) -> Response {
    let Some(web_dir) = state.web_dir else {
        return web::index();
    };
    let index = web_dir.join("index.html");
    match ServeFile::new(index)
        .oneshot(Request::new(Body::empty()))
        .await
    {
        Ok(res) => res.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn app_info() -> Json<ApiResult<AppInfo>> {
    ApiResult::from_result(Ok(service::app_info()))
}

async fn recent_logs() -> Json<ApiResult<Vec<LogEntry>>> {
    ApiResult::from_result(Ok(service::get_recent_logs()))
}

async fn telegram_status(State(ctx): State<AppCtx>) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::get_telegram_status(&ctx, false)).await
}

async fn connect_telegram(State(ctx): State<AppCtx>) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::connect_telegram(&ctx, false)).await
}

#[derive(Deserialize)]
struct PhoneBody {
    phone: String,
}

async fn request_login_code(
    State(ctx): State<AppCtx>,
    Json(body): Json<PhoneBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::request_login_code(&ctx, body.phone, false)).await
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn submit_login_code(
    State(ctx): State<AppCtx>,
    Json(body): Json<CodeBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::submit_login_code(&ctx, body.code, false)).await
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

async fn submit_password(
    State(ctx): State<AppCtx>,
    Json(body): Json<PasswordBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::submit_password(&ctx, body.password, false)).await
}

async fn logout(State(ctx): State<AppCtx>) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::logout(&ctx, false)).await
}

#[derive(Deserialize)]
struct ChatsQuery {
    refresh: Option<bool>,
}

async fn list_chats(
    State(ctx): State<AppCtx>,
    Query(q): Query<ChatsQuery>,
) -> Json<ApiResult<Vec<ChatItem>>> {
    wrap(service::list_chats(&ctx, q.refresh.unwrap_or(false))).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchedBody {
    chat_id: String,
    watched: bool,
}

async fn set_chat_watched(
    State(ctx): State<AppCtx>,
    Json(body): Json<WatchedBody>,
) -> Json<ApiResult<bool>> {
    wrap(service::set_chat_watched(&ctx, body.chat_id, body.watched)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuestWatchBody {
    enabled: bool,
    #[serde(default)]
    query: String,
}

async fn set_guest_watch(
    State(ctx): State<AppCtx>,
    Json(body): Json<GuestWatchBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::set_guest_watch(
        &ctx,
        false,
        body.enabled,
        body.query,
    ))
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddGuestWatchBody {
    query: String,
}

async fn add_guest_watch(
    State(ctx): State<AppCtx>,
    Json(body): Json<AddGuestWatchBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::add_guest_watch(&ctx, false, body.query)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveGuestWatchBody {
    chat_id: String,
}

async fn remove_guest_watch(
    State(ctx): State<AppCtx>,
    Json(body): Json<RemoveGuestWatchBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::remove_guest_watch(&ctx, false, body.chat_id)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetGuestWatchEnabledBody {
    chat_id: String,
    enabled: bool,
}

async fn set_guest_watch_enabled(
    State(ctx): State<AppCtx>,
    Json(body): Json<SetGuestWatchEnabledBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::set_guest_watch_enabled(
        &ctx,
        false,
        body.chat_id,
        body.enabled,
    ))
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypesBody {
    chat_id: String,
    types: ChatDownloadTypes,
}

async fn set_chat_download_types(
    State(ctx): State<AppCtx>,
    Json(body): Json<TypesBody>,
) -> Json<ApiResult<ChatDownloadTypes>> {
    ApiResult::from_result(service::set_chat_download_types(
        &ctx,
        body.chat_id,
        body.types,
    ))
}

#[derive(Deserialize)]
struct DaysBody {
    days: i32,
}

async fn set_backfill_days(
    State(ctx): State<AppCtx>,
    Json(body): Json<DaysBody>,
) -> Json<ApiResult<i32>> {
    ApiResult::from_result(service::set_backfill_days(&ctx, body.days))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatDaysBody {
    chat_id: String,
    days: Option<i32>,
}

async fn set_chat_backfill_days(
    State(ctx): State<AppCtx>,
    Json(body): Json<ChatDaysBody>,
) -> Json<ApiResult<i32>> {
    ApiResult::from_result(service::set_chat_backfill_days(
        &ctx,
        body.chat_id,
        body.days,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AliasBody {
    chat_id: String,
    alias: Option<String>,
}

async fn set_chat_alias(
    State(ctx): State<AppCtx>,
    Json(body): Json<AliasBody>,
) -> Json<ApiResult<Option<String>>> {
    ApiResult::from_result(service::set_chat_alias(&ctx, body.chat_id, body.alias))
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn set_show_media(
    State(ctx): State<AppCtx>,
    Json(body): Json<EnabledBody>,
) -> Json<ApiResult<bool>> {
    ApiResult::from_result(service::set_show_media(&ctx, body.enabled))
}

async fn set_proxy(
    State(ctx): State<AppCtx>,
    Json(body): Json<ProxyConfig>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::set_proxy(&ctx, false, body)).await
}

#[derive(Deserialize)]
struct DownloadDirBody {
    dir: String,
}

async fn set_download_dir(
    State(ctx): State<AppCtx>,
    Json(body): Json<DownloadDirBody>,
) -> Json<ApiResult<TelegramStatus>> {
    wrap(service::set_download_dir(&ctx, false, body.dir)).await
}

async fn download_dir_options(
    State(ctx): State<AppCtx>,
) -> Json<ApiResult<service::DownloadDirOptions>> {
    ApiResult::from_result(Ok(service::download_dir_options(&ctx)))
}

#[derive(Deserialize)]
struct ConcurrencyBody {
    n: u32,
}

async fn set_download_concurrency(
    State(ctx): State<AppCtx>,
    Json(body): Json<ConcurrencyBody>,
) -> Json<ApiResult<u32>> {
    ApiResult::from_result(service::set_download_concurrency(&ctx, body.n))
}

#[derive(Deserialize)]
struct MinMediaMbBody {
    n: f64,
}

async fn set_min_media_mb(
    State(ctx): State<AppCtx>,
    Json(body): Json<MinMediaMbBody>,
) -> Json<ApiResult<f64>> {
    ApiResult::from_result(service::set_min_media_mb(&ctx, body.n))
}

async fn set_autostart_noop() -> Json<ApiResult<bool>> {
    ApiResult::from_result(Ok(false))
}

#[derive(Deserialize)]
struct PausedBody {
    paused: bool,
}

async fn set_download_paused(
    State(ctx): State<AppCtx>,
    Json(body): Json<PausedBody>,
) -> Json<ApiResult<bool>> {
    ApiResult::from_result(service::set_download_paused(&ctx, body.paused))
}

async fn ilink_status(State(ctx): State<AppCtx>) -> Json<ApiResult<IlinkStatus>> {
    ApiResult::from_result(Ok(service::get_ilink_status(&ctx)))
}

async fn ilink_start_login(State(ctx): State<AppCtx>) -> Json<ApiResult<IlinkStatus>> {
    wrap(service::ilink_start_login(&ctx)).await
}

async fn ilink_logout(State(ctx): State<AppCtx>) -> Json<ApiResult<IlinkStatus>> {
    ApiResult::from_result(service::ilink_logout(&ctx))
}

async fn set_ilink_notify_enabled(
    State(ctx): State<AppCtx>,
    Json(body): Json<EnabledBody>,
) -> Json<ApiResult<IlinkStatus>> {
    ApiResult::from_result(service::set_ilink_notify_enabled(&ctx, body.enabled))
}

async fn ilink_send_test(State(ctx): State<AppCtx>) -> Json<ApiResult<IlinkStatus>> {
    wrap(service::ilink_send_test(&ctx)).await
}

async fn download_status(State(ctx): State<AppCtx>) -> Json<ApiResult<DownloadProgress>> {
    ApiResult::from_result(Ok(service::get_download_status(&ctx)))
}

async fn list_downloads(State(ctx): State<AppCtx>) -> Json<ApiResult<Vec<DownloadItem>>> {
    ApiResult::from_result(service::list_downloads(&ctx))
}

async fn download_usage(State(ctx): State<AppCtx>) -> Json<ApiResult<DownloadUsage>> {
    ApiResult::from_result(service::get_download_usage(&ctx))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileIdBody {
    file_id: String,
}

async fn cancel_download(
    State(ctx): State<AppCtx>,
    Json(body): Json<FileIdBody>,
) -> Json<ApiResult<bool>> {
    ApiResult::from_result(service::cancel_download(&ctx, body.file_id))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessagesQuery {
    chat_id: String,
    query: Option<String>,
    before_message_id: Option<i32>,
    limit: Option<i32>,
}

async fn list_messages(
    State(ctx): State<AppCtx>,
    Query(q): Query<MessagesQuery>,
) -> Json<ApiResult<MessagePage>> {
    wrap(service::list_messages(
        &ctx,
        q.chat_id,
        q.query,
        q.before_message_id,
        q.limit,
    ))
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    query: String,
    date_unix: Option<String>,
    chat_id: Option<String>,
    message_id: Option<i32>,
    limit: Option<i32>,
}

async fn search_messages(
    State(ctx): State<AppCtx>,
    Query(q): Query<SearchQuery>,
) -> Json<ApiResult<MessagePage>> {
    let cursor = match (q.date_unix, q.chat_id, q.message_id) {
        (Some(date_unix), Some(chat_id), Some(message_id)) => Some(SearchCursor {
            date_unix,
            chat_id,
            message_id,
        }),
        _ => None,
    };
    wrap(service::search_messages(&ctx, q.query, cursor, q.limit)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearBody {
    chat_id: String,
}

async fn clear_chat_messages(
    State(ctx): State<AppCtx>,
    Json(body): Json<ClearBody>,
) -> Json<ApiResult<u32>> {
    wrap(service::clear_chat_messages(&ctx, body.chat_id)).await
}

async fn check_chat_media(
    State(ctx): State<AppCtx>,
    Json(body): Json<ClearBody>,
) -> Json<ApiResult<bool>> {
    wrap(service::check_chat_media(&ctx, body.chat_id)).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RedownloadBody {
    chat_id: String,
    message_id: i32,
}

async fn redownload_message_media(
    State(ctx): State<AppCtx>,
    Json(body): Json<RedownloadBody>,
) -> Json<ApiResult<bool>> {
    wrap(service::redownload_message_media(
        &ctx,
        body.chat_id,
        body.message_id,
    ))
    .await
}

#[derive(Deserialize)]
struct UrlBody {
    url: String,
}

async fn open_url(Json(body): Json<UrlBody>) -> Json<ApiResult<()>> {
    if crate::telegram::sanitize_http_url(&body.url).is_none() {
        return ApiResult::from_result(Err(AppError::Config("只允许打开 http/https 链接".into())));
    }
    ApiResult::from_result(Ok(()))
}

async fn events(State(ctx): State<AppCtx>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut status = ctx.events.subscribe_telegram_status();
    let mut progress = ctx.events.subscribe_download_progress();
    let mut ingested = ctx.events.subscribe_chat_ingested();
    let mut ilink = ctx.events.subscribe_ilink_status();
    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                result = status.recv() => {
                    if let Ok(payload) = result {
                        yield Ok(sse_json("telegram-status-changed", &payload));
                    }
                }
                result = progress.recv() => {
                    if let Ok(payload) = result {
                        yield Ok(sse_json("download-progress", &payload));
                    }
                }
                result = ingested.recv() => {
                    if let Ok(payload) = result {
                        yield Ok(sse_json("chat-ingested", &payload));
                    }
                }
                result = ilink.recv() => {
                    if let Ok(payload) = result {
                        yield Ok(sse_json("ilink-status", &payload));
                    }
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn sse_json<T: Serialize>(name: &str, payload: &T) -> Event {
    Event::default()
        .event(name)
        .data(serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()))
}

#[derive(Deserialize)]
struct MediaQuery {
    path: String,
}

async fn serve_download_file(target: PathBuf, req: Request) -> Response {
    match ServeFile::new(target).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn resolve_media_file(ctx: &AppCtx, raw: &str) -> Result<PathBuf, AppError> {
    let paths = ctx.paths();
    paths.ensure_dirs()?;
    let target = service::resolve_under_dir(&paths.download_dir, raw)?;
    if !target.is_file() {
        return Err(AppError::Io("不是文件".into()));
    }
    Ok(target)
}

fn media_error(err: AppError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiResult::<()>::Error { error: err }),
    )
        .into_response()
}

async fn media(State(ctx): State<AppCtx>, Query(q): Query<MediaQuery>, req: Request) -> Response {
    match resolve_media_file(&ctx, &q.path) {
        Ok(target) => serve_download_file(target, req).await,
        Err(err) => media_error(err),
    }
}

async fn media_thumb(
    State(ctx): State<AppCtx>,
    Query(q): Query<MediaQuery>,
    req: Request,
) -> Response {
    let target = match resolve_media_file(&ctx, &q.path) {
        Ok(path) => path,
        Err(err) => return media_error(err),
    };
    let data_root = ctx.paths().root.clone();
    match crate::thumb::ensure_jpeg_thumb(&data_root, &target).await {
        Ok(Some(thumb)) => {
            let mut response = serve_download_file(thumb, req).await;
            response.headers_mut().insert(
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            response
        }
        Ok(None) | Err(_) => serve_download_file(target, req).await,
    }
}
