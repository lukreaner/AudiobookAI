use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use audiobookai_service::{ServiceConfig, ServiceHandle};
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug)]
struct DesktopState {
    service: Mutex<Option<ServiceHandle>>,
    quitting: AtomicBool,
    close_to_tray: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseAction {
    Allow,
    Hide,
    Exit,
}

const fn close_action(quitting: bool, close_to_tray: bool) -> CloseAction {
    if quitting {
        CloseAction::Allow
    } else if close_to_tray {
        CloseAction::Hide
    } else {
        CloseAction::Exit
    }
}

fn installed_sidecar_directory(resource_dir: &Path) -> Option<PathBuf> {
    let directory = resource_dir.join("sidecars").join("bin");
    directory.is_dir().then_some(directory)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn set_close_to_tray(state: tauri::State<'_, DesktopState>, enabled: bool) {
    state.close_to_tray.store(enabled, Ordering::SeqCst);
}

fn begin_quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<DesktopState>() {
        state.quitting.store(true, Ordering::SeqCst);
    }
    app.exit(0);
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn quit_application(app: AppHandle) {
    begin_quit(&app);
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn emit_epub_open(app: &AppHandle, arguments: &[String]) {
    if let Some(path) = arguments
        .iter()
        .find(|argument| argument.to_ascii_lowercase().ends_with(".epub"))
    {
        show_main_window(app);
        let _ = app.emit("audiobookai://open-epub", path);
    }
}

fn create_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open AudiobookAI", true, None::<&str>)?;
    let import = MenuItem::with_id(app, "import", "Import EPUB…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit AudiobookAI", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &import, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main-tray")
        .tooltip("AudiobookAI")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "import" => {
                show_main_window(app);
                let _ = app.emit("audiobookai://show-import", ());
            }
            "quit" => begin_quit(app),
            _ => {}
        });

    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn shutdown_service(app: &AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else {
        return;
    };
    let handle = state.service.lock().ok().and_then(|mut value| value.take());
    if let Some(handle) = handle
        && let Err(error) = tauri::async_runtime::block_on(handle.shutdown())
    {
        tracing::warn!(diagnostic_code = "desktop.service.shutdown.failed", %error, "desktop service did not shut down cleanly");
    }
}

fn desktop_service_authority(service: &ServiceHandle) -> String {
    let port = service.address().port();
    if let Some(host) = service.state().config.lan_hostnames.first() {
        return match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(address)) => format!("[{address}]:{port}"),
            _ => format!("{host}:{port}"),
        };
    }
    match service.address().ip() {
        std::net::IpAddr::V6(address) => format!("[{address}]:{port}"),
        address @ std::net::IpAddr::V4(_) => format!("{address}:{port}"),
    }
}

/// Starts the native desktop host and its in-process service.
///
/// # Panics
///
/// Panics if Tauri cannot construct the application after configuration has
/// already been validated. Runtime service-start failures are returned through
/// Tauri's setup error path instead.
#[allow(clippy::too_many_lines)]
pub fn run() {
    // The installed app intentionally has no raw formatting layer: ordinary
    // tracing fields can contain provider errors or payload fragments. The
    // diagnostics layer retains only approved static messages and strictly
    // allowlisted scalar metadata for the authenticated dashboard.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(audiobookai_service::diagnostics::DiagnosticsLayer)
        .init();

    let application = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, arguments, _working_directory| {
            emit_epub_open(app, &arguments);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![quit_application, set_close_to_tray])
        .setup(|app| {
            let mut loopback_config = ServiceConfig::desktop_default()?;
            loopback_config.bundled_sidecar_dir =
                installed_sidecar_directory(&app.path().resource_dir()?);
            let (service_config, persisted_lan_applied) = match tauri::async_runtime::block_on(
                loopback_config
                    .clone()
                    .with_persisted_desktop_lan_settings(),
            ) {
                Ok(config) => {
                    let applied = config.bind != loopback_config.bind
                        || config.tls.is_some()
                        || !config.lan_hostnames.is_empty()
                        || config.allow_insecure_lan;
                    (config, applied)
                }
                Err(error) => {
                    tracing::error!(diagnostic_code = "desktop.lan.settings.rejected", %error, "persisted LAN settings were rejected; using loopback-only recovery mode");
                    (loopback_config.clone(), false)
                }
            };
            let service = match tauri::async_runtime::block_on(audiobookai_service::start(
                service_config,
            )) {
                Ok(service) => service,
                Err(error) if persisted_lan_applied => {
                    tracing::error!(diagnostic_code = "desktop.lan.listener.failed", %error, "persisted LAN listener failed to start; using loopback-only recovery mode");
                    tauri::async_runtime::block_on(audiobookai_service::start(loopback_config))?
                }
                Err(error) => return Err(error.into()),
            };
            let origin = format!(
                "{}://{}",
                service.scheme(),
                desktop_service_authority(&service)
            );
            let bootstrap_nonce = tauri::async_runtime::block_on(
                service.desktop_bootstrap_nonce(),
            );
            let bootstrap_javascript = serde_json::to_string(&bootstrap_nonce.as_deref())?;
            let initial_epub = std::env::args().find(|argument| {
                argument.to_ascii_lowercase().ends_with(".epub")
            });
            let initial_epub_javascript = serde_json::to_string(&initial_epub)?;
            app.manage(DesktopState {
                service: Mutex::new(Some(service)),
                quitting: AtomicBool::new(false),
                close_to_tray: AtomicBool::new(true),
            });

            let initialization_script = format!(
                "window.__AUDIOBOOKAI_API__ = {origin:?}; window.__AUDIOBOOKAI_BOOTSTRAP__ = {bootstrap_javascript}; window.__AUDIOBOOKAI_OPEN_EPUB__ = {initial_epub_javascript};",
            );
            WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(origin.parse()?),
            )
                .title("AudiobookAI")
                .inner_size(1280.0, 820.0)
                .min_inner_size(920.0, 640.0)
                .resizable(true)
                .initialization_script(&initialization_script)
                .build()?;
            create_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let Some(state) = window.app_handle().try_state::<DesktopState>() else {
                    return;
                };
                match close_action(
                    state.quitting.load(Ordering::SeqCst),
                    state.close_to_tray.load(Ordering::SeqCst),
                ) {
                    CloseAction::Allow => {}
                    CloseAction::Hide => {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    CloseAction::Exit => {
                        api.prevent_close();
                        begin_quit(window.app_handle());
                    }
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build AudiobookAI desktop host");

    application.run(|app, event| match event {
        RunEvent::ExitRequested { .. } | RunEvent::Exit => shutdown_service(app),
        #[cfg(target_os = "macos")]
        RunEvent::Opened { urls } => {
            let arguments = urls
                .iter()
                .map(|url| {
                    url.to_file_path().map_or_else(
                        |()| url.to_string(),
                        |path| path.to_string_lossy().into_owned(),
                    )
                })
                .collect::<Vec<_>>();
            emit_epub_open(app, &arguments);
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main_window(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn close_policy_preserves_tray_jobs_only_when_enabled() {
        assert_eq!(close_action(false, true), CloseAction::Hide);
        assert_eq!(close_action(false, false), CloseAction::Exit);
        assert_eq!(close_action(true, true), CloseAction::Allow);
    }

    #[test]
    fn installed_sidecars_resolve_from_the_preserved_bundle_tree() {
        let resources = TempDir::new().expect("resource directory");
        let bin = resources.path().join("sidecars").join("bin");
        std::fs::create_dir_all(&bin).expect("sidecar bin directory");

        assert_eq!(installed_sidecar_directory(resources.path()), Some(bin));
        assert!(installed_sidecar_directory(&resources.path().join("missing")).is_none());
    }

    #[test]
    fn base_updater_config_is_secret_free_and_deserializable() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("valid Tauri config");
        let updater = config
            .pointer("/plugins/updater")
            .and_then(serde_json::Value::as_object)
            .expect("base updater configuration must be an object");

        assert_eq!(updater.get("endpoints"), Some(&serde_json::json!([])));
        assert_eq!(updater.get("pubkey"), Some(&serde_json::json!("")));
    }
}
