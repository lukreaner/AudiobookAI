use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

mod storage_location;

use audiobookai_service::{ServiceConfig, ServiceHandle};
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_dialog::DialogExt as _;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug)]
struct DesktopState {
    service: Mutex<Option<ServiceHandle>>,
    storage_config_path: PathBuf,
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

#[tauri::command]
async fn choose_storage_directory(
    app: AppHandle,
    starting_directory: Option<String>,
) -> Result<Option<String>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose AudiobookAI storage folder")
        .set_can_create_directories(true);
    if let Some(directory) = starting_directory
        .map(PathBuf::from)
        .filter(|directory| directory.is_dir())
    {
        dialog = dialog.set_directory(directory);
    }
    let selected = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_folder())
        .await
        .map_err(|error| format!("the storage-folder dialog failed: {error}"))?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected = selected
        .into_path()
        .map_err(|error| format!("the selected storage folder is invalid: {error}"))?;
    selected
        .into_os_string()
        .into_string()
        .map(Some)
        .map_err(|_| "the selected storage folder is not valid Unicode".to_owned())
}

#[tauri::command]
#[allow(clippy::too_many_lines)]
async fn relocate_first_run_storage(
    state: tauri::State<'_, DesktopState>,
    data_root: String,
) -> Result<(), String> {
    let app_state = {
        let service = state
            .service
            .lock()
            .map_err(|_| "the desktop service state is unavailable".to_owned())?;
        service
            .as_ref()
            .ok_or_else(|| "the desktop service is not running".to_owned())?
            .state()
            .clone()
    };
    {
        let catalog = app_state.catalog.read().await;
        if catalog.settings.first_run_complete {
            return Err(
                "storage can only be changed here before first-run setup is completed".to_owned(),
            );
        }
        if !catalog.projects.is_empty()
            || !catalog.import_drafts.is_empty()
            || !catalog.jobs.is_empty()
            || !catalog.exports.is_empty()
            || !catalog.usage_rows.is_empty()
        {
            return Err(
                "storage cannot be changed after project or job data has been created".to_owned(),
            );
        }
    }
    let original_config = app_state.config.clone();
    drop(app_state);

    let service = state
        .service
        .lock()
        .map_err(|_| "the desktop service state is unavailable".to_owned())?
        .take()
        .ok_or_else(|| "the desktop service is not running".to_owned())?;
    if let Err(error) = service.shutdown().await {
        let message = format!("could not stop the desktop service for relocation: {error}");
        return Err(recover_original_service(&state, original_config, message).await);
    }

    let source = original_config.data_dir.clone();
    let staged = match tauri::async_runtime::spawn_blocking(move || {
        storage_location::stage_relocation(&source, &data_root)
    })
    .await
    {
        Ok(Ok(staged)) => staged,
        Ok(Err(error)) => {
            return Err(recover_original_service(&state, original_config, error).await);
        }
        Err(error) => {
            let message = format!("the storage relocation task failed: {error}");
            return Err(recover_original_service(&state, original_config, message).await);
        }
    };
    let storage_location::StagedRelocation::Ready { target, marker } = staged else {
        start_and_store_service(&state, original_config).await?;
        return Ok(());
    };

    let mut relocated_config = original_config.clone();
    relocated_config.data_dir.clone_from(&target);
    let relocated_service = match audiobookai_service::start(relocated_config).await {
        Ok(service) => service,
        Err(error) => {
            let target_for_rollback = target.clone();
            let marker_for_rollback = marker.clone();
            let rollback = tauri::async_runtime::spawn_blocking(move || {
                storage_location::rollback_relocation(&target_for_rollback, &marker_for_rollback)
            })
            .await;
            let mut message = format!("could not open the relocated storage folder: {error}");
            if let Ok(Err(rollback_error)) = rollback {
                let _ = write!(message, "; {rollback_error}");
            }
            return Err(recover_original_service(&state, original_config, message).await);
        }
    };

    let config_path = state.storage_config_path.clone();
    let target_for_config = target.clone();
    let config_result = tauri::async_runtime::spawn_blocking(move || {
        storage_location::persist_data_root(&config_path, &target_for_config)
    })
    .await
    .map_err(|error| format!("the storage configuration task failed: {error}"))
    .and_then(|result| result);
    if let Err(error) = config_result {
        if let Err(shutdown_error) = relocated_service.shutdown().await {
            return Err(format!(
                "{error}; the relocated service did not stop cleanly: {shutdown_error}. Restart AudiobookAI."
            ));
        }
        let target_for_rollback = target.clone();
        let marker_for_rollback = marker.clone();
        let rollback_error = tauri::async_runtime::spawn_blocking(move || {
            storage_location::rollback_relocation(&target_for_rollback, &marker_for_rollback)
        })
        .await
        .ok()
        .and_then(Result::err);
        let mut message = error;
        if let Some(rollback_error) = rollback_error {
            let _ = write!(message, "; {rollback_error}");
        }
        return Err(recover_original_service(&state, original_config, message).await);
    }

    let target_for_finish = target.clone();
    let marker_for_finish = marker.clone();
    if let Ok(Err(error)) = tauri::async_runtime::spawn_blocking(move || {
        storage_location::finish_relocation(&target_for_finish, &marker_for_finish)
    })
    .await
    {
        tracing::warn!(
            diagnostic_code = "desktop.storage.relocation_marker_cleanup.failed",
            %error,
            "the storage root was switched but its relocation marker could not be removed"
        );
    }
    store_service(&state, relocated_service)
}

async fn recover_original_service(
    state: &DesktopState,
    config: ServiceConfig,
    message: String,
) -> String {
    match start_and_store_service(state, config).await {
        Ok(()) => message,
        Err(restart_error) => format!(
            "{message}; the original service could not be restarted: {restart_error}. Restart AudiobookAI."
        ),
    }
}

async fn start_and_store_service(
    state: &DesktopState,
    config: ServiceConfig,
) -> Result<(), String> {
    let service = audiobookai_service::start(config)
        .await
        .map_err(|error| format!("could not start the desktop service: {error}"))?;
    store_service(state, service)
}

fn store_service(state: &DesktopState, service: ServiceHandle) -> Result<(), String> {
    let mut stored = state
        .service
        .lock()
        .map_err(|_| "the desktop service state is unavailable".to_owned())?;
    *stored = Some(service);
    Ok(())
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

#[cfg(target_os = "linux")]
fn configure_appimage_renderer(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};

    window.with_webview(|webview| {
        if let Some(settings) = webview.inner().settings() {
            settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
        }
    })
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
        .invoke_handler(tauri::generate_handler![
            choose_storage_directory,
            quit_application,
            relocate_first_run_storage,
            set_close_to_tray
        ])
        .setup(|app| {
            let mut loopback_config = ServiceConfig::desktop_default()?;
            let storage_config_path = app
                .path()
                .app_config_dir()?
                .join("storage-location.json");
            loopback_config.data_dir = storage_location::configured_data_root(
                &storage_config_path,
                &loopback_config.data_dir,
            )
            .map_err(std::io::Error::other)?;
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
                storage_config_path,
                quitting: AtomicBool::new(false),
                close_to_tray: AtomicBool::new(true),
            });

            let initialization_script = format!(
                "window.__AUDIOBOOKAI_API__ = {origin:?}; window.__AUDIOBOOKAI_BOOTSTRAP__ = {bootstrap_javascript}; window.__AUDIOBOOKAI_OPEN_EPUB__ = {initial_epub_javascript};",
            );
            let origin_url: tauri::Url = origin.parse()?;
            #[cfg(target_os = "linux")]
            let use_software_renderer = std::env::var_os("APPIMAGE").is_some();
            #[cfg(not(target_os = "linux"))]
            let use_software_renderer = false;
            let initial_url = if use_software_renderer {
                WebviewUrl::External("about:blank".parse()?)
            } else {
                WebviewUrl::External(origin_url.clone())
            };
            let main_window = WebviewWindowBuilder::new(
                app,
                "main",
                initial_url,
            )
                .title("AudiobookAI")
                .inner_size(1280.0, 820.0)
                .min_inner_size(920.0, 640.0)
                .resizable(true)
                .visible(!use_software_renderer)
                .initialization_script(&initialization_script)
                .build()?;
            #[cfg(target_os = "linux")]
            if use_software_renderer {
                configure_appimage_renderer(&main_window)?;
                main_window.navigate(origin_url)?;
                main_window.show()?;
                tracing::info!(
                    diagnostic_code = "desktop.renderer.software",
                    "using software rendering for Linux AppImage compatibility"
                );
            }
            #[cfg(not(target_os = "linux"))]
            let _ = main_window;
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
