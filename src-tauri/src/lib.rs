mod app_log;
mod commands;
mod error;
mod ilink;
mod runtime;
mod service;
mod settings;
mod telegram;

#[cfg(feature = "desktop")]
mod tray;

#[cfg(feature = "server")]
mod thumb;

#[cfg(feature = "server")]
pub mod server;

use runtime::AppCtx;

pub struct AppState {
    pub ctx: AppCtx,
}

#[cfg(feature = "desktop")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    use commands::{
        add_guest_watch, cancel_download, check_chat_media, clear_chat_messages, connect_telegram,
        get_app_info, get_download_status, get_download_usage, get_ilink_status, get_recent_logs,
        get_telegram_status, hide_main_window, ilink_logout, ilink_send_test, ilink_start_login,
        list_chats, list_downloads, list_messages, logout, open_download_dir, open_path, open_url,
        pick_download_dir, quit_app, redownload_message_media, remove_guest_watch,
        request_login_code, search_messages, set_autostart, set_backfill_days, set_chat_alias,
        set_chat_backfill_days, set_chat_download_types, set_chat_watched,
        set_download_concurrency, set_download_dir, set_download_paused, set_guest_watch,
        set_guest_watch_enabled, set_ilink_notify_enabled, set_min_media_mb, set_proxy,
        set_show_media, show_main_window, submit_login_code, submit_password,
    };
    use ilink::{IlinkQrState, IlinkStatus};
    use settings::{ChatDownloadTypes, GuestWatchEntry, GuestWatchStatus, ProxyConfig};
    use tauri_specta::{collect_commands, collect_events, Builder};
    use telegram::{
        AccountInfo, ActiveDownload, ChatIngested, ChatItem, ChatKind, DownloadPhase,
        DownloadProgress, LoginStep, MediaKind, QueuedDownload, TelegramStatusChanged,
    };
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            get_app_info,
            get_recent_logs,
            get_telegram_status,
            connect_telegram,
            request_login_code,
            submit_login_code,
            submit_password,
            logout,
            list_chats,
            set_chat_watched,
            set_guest_watch,
            add_guest_watch,
            remove_guest_watch,
            set_guest_watch_enabled,
            set_chat_download_types,
            set_backfill_days,
            set_chat_backfill_days,
            set_chat_alias,
            set_show_media,
            set_proxy,
            set_autostart,
            set_download_concurrency,
            set_min_media_mb,
            get_download_status,
            set_download_paused,
            get_ilink_status,
            ilink_start_login,
            ilink_logout,
            set_ilink_notify_enabled,
            ilink_send_test,
            cancel_download,
            list_downloads,
            get_download_usage,
            list_messages,
            search_messages,
            clear_chat_messages,
            check_chat_media,
            redownload_message_media,
            open_url,
            open_path,
            open_download_dir,
            pick_download_dir,
            set_download_dir,
            show_main_window,
            hide_main_window,
            quit_app
        ])
        .events(collect_events![
            TelegramStatusChanged,
            DownloadProgress,
            ChatIngested,
            IlinkStatus
        ])
        .typ::<ProxyConfig>()
        .typ::<GuestWatchStatus>()
        .typ::<GuestWatchEntry>()
        .typ::<MediaKind>()
        .typ::<ChatItem>()
        .typ::<ChatKind>()
        .typ::<LoginStep>()
        .typ::<AccountInfo>()
        .typ::<ChatDownloadTypes>()
        .typ::<DownloadPhase>()
        .typ::<ActiveDownload>()
        .typ::<QueuedDownload>()
        .typ::<IlinkQrState>()
        .typ::<IlinkStatus>()
}

#[cfg(feature = "desktop")]
fn spawn_tauri_event_bridge(app: tauri::AppHandle, events: crate::runtime::EventHub) {
    use tauri_specta::Event;
    tauri::async_runtime::spawn(async move {
        let mut status = events.subscribe_telegram_status();
        let mut progress = events.subscribe_download_progress();
        let mut ingested = events.subscribe_chat_ingested();
        let mut ilink = events.subscribe_ilink_status();
        loop {
            tokio::select! {
                Ok(payload) = status.recv() => {
                    let _ = payload.emit(&app);
                }
                Ok(payload) = progress.recv() => {
                    let _ = payload.emit(&app);
                }
                Ok(payload) = ingested.recv() => {
                    let _ = payload.emit(&app);
                }
                Ok(payload) = ilink.recv() => {
                    let _ = payload.emit(&app);
                }
            }
        }
    });
}

#[cfg(feature = "desktop")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use commands::apply_autostart;
    use settings::AppSettings;
    use specta_typescript::Typescript;
    use tauri::Manager;
    use telegram::load_dotenv;

    load_dotenv();
    crate::app_log::init();

    let specta = specta_builder();

    #[cfg(debug_assertions)]
    specta
        .export(
            Typescript::default().header("// @generated by tauri-specta. Do not edit.\n"),
            "../src/lib/bindings.ts",
        )
        .expect("failed to export typescript bindings");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .skip_logger()
                .build(),
        )
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            specta.mount_events(app);
            tray::setup(app)?;

            let data_root = app.path().app_data_dir().map_err(|err| err.to_string())?;
            let ctx = AppCtx::new(data_root, None);
            let paths = ctx.paths();
            if let Err(err) = paths.ensure_dirs() {
                log::warn!("failed to create telegram data dirs: {err}");
            }
            paths.allow_asset_access(app.handle());
            let settings = AppSettings::load(&paths.root);
            if let Err(err) = apply_autostart(app.handle(), settings.autostart) {
                log::warn!("failed to apply autostart: {err}");
            }
            ctx.sync.set_paused(settings.download_paused);
            crate::ilink::spawn_ilink_worker(ctx.clone());
            spawn_tauri_event_bridge(app.handle().clone(), ctx.events.clone());
            app.manage(AppState { ctx });

            if std::env::args().any(|arg| arg == "--hidden") {
                tray::hide_window(app.handle());
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use specta_typescript::Typescript;

    #[test]
    fn export_typescript_bindings() {
        specta_builder()
            .export(
                Typescript::default().header("// @generated by tauri-specta. Do not edit.\n"),
                "../src/lib/bindings.ts",
            )
            .expect("failed to export typescript bindings");
    }
}
