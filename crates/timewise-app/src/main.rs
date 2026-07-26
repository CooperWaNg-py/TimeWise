#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! TimeWise desktop shell: one binary, role per OS account (master | worker),
//! selected on first run (US-01). Master spawns the embedded API + mDNS;
//! worker spawns the tracking runtime with OS-native notifications.

use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_notification::NotificationExt;
use timewise_app::config::{Config, MasterRegistration, Role};
use timewise_app::master_server::{self, AppState};
use timewise_app::notify::Notifier;
use timewise_app::tracker::ActiveWinPosRs;
use timewise_app::worker_runtime::{self, WorkerRuntime};
use timewise_core::model::RegisterRequest;
use timewise_core::store::master as master_store;

struct Shared {
    dir: PathBuf,
    worker_shutdown: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

#[derive(Serialize, Clone)]
struct UiState {
    role: Option<String>,
    port: u16,
    worker_id: String,
    masters: Vec<MasterRegistration>,
    track_self: bool,
    idle_threshold_s: u64,
}

struct TauriNotifier(AppHandle);

impl Notifier for TauriNotifier {
    fn notify(&mut self, title: &str, body: &str) {
        if let Err(e) = self.0.notification().builder().title(title).body(body).show() {
            eprintln!("[timewise] notification failed: {e}");
        }
    }
}

fn local_tz_offset_s() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}

fn start_master(app: &AppHandle, shared: &Shared, cfg: Config) {
    let dir = shared.dir.clone();
    enable_autostart();
    if cfg.track_self {
        start_self_worker(app, shared, &cfg);
    }
    tauri::async_runtime::spawn(async move {
        let conn = match master_store::open(&Config::master_db_path(&dir)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[timewise] master db open failed: {e}");
                return;
            }
        };
        let state = AppState::new(conn, local_tz_offset_s(), cfg.break_prompt_after_min);
        let app = master_server::router(state.clone());
        let mdns = timewise_app::mdns_announcer::announce(cfg.port);
        if mdns.is_some() {
            println!("[timewise] master announcing on _timewise._tcp.local. port {}", cfg.port);
        }

        // Hourly points evaluation (also runs on dashboard load).
        let pts = state.clone();
        tauri::async_runtime::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                iv.tick().await;
                let now = chrono::Utc::now().timestamp();
                let db = pts.db.lock();
                if let Err(e) = timewise_app::points_engine::evaluate_all(&db, now, pts.tz_offset_s) {
                    eprintln!("[timewise] points evaluation failed: {e}");
                }
            }
        });

        match tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await {
            Ok(listener) => {
                println!("[timewise] master API listening on 0.0.0.0:{}", cfg.port);
                if let Err(e) = axum::serve(listener, app).await {
                    eprintln!("[timewise] server error: {e}");
                }
            }
            Err(e) => eprintln!("[timewise] cannot bind port {}: {e}", cfg.port),
        }
    });
}

/// Optional self-tracking on a master: register a worker pointed at our own
/// server. It appears as a pending device; the parent approves/merges it like
/// any other.
fn start_self_worker(app: &AppHandle, shared: &Shared, cfg: &Config) {
    let dir = shared.dir.clone();
    let token = cfg.self_token.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if cfg.self_token.is_none() {
        let mut cfg2 = cfg.clone();
        cfg2.self_token = Some(token.clone());
        let _ = cfg2.save_to(&dir);
    }
    if let Ok(conn) = master_store::open(&Config::master_db_path(&dir)) {
        let req = RegisterRequest {
            worker_id: format!("{}-self", cfg.worker_id),
            hostname: machine_hostname(),
            os: std::env::consts::OS.into(),
            os_user: os_username(),
            token: token.clone(),
        };
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = master_store::upsert_worker(&conn, &req, now) {
            eprintln!("[timewise] self-track registration failed: {e}");
        }
    }
    let mut worker_cfg = cfg.clone();
    worker_cfg.worker_id = format!("{}-self", cfg.worker_id);
    worker_cfg.masters = vec![MasterRegistration {
        base_url: format!("http://127.0.0.1:{}", cfg.port),
        token,
    }];
    start_worker(app, shared, worker_cfg);
}

fn enable_autostart() {
    if let Ok(exe) = std::env::current_exe() {
        // auto-launch's constructor is OS-dependent (macOS takes use_launch_agent).
        // macOS: LaunchAgent works for unpackaged binaries; the AppleScript
        // login-item path (false) requires an .app bundle and silently fails.
        #[cfg(target_os = "macos")]
        let auto = auto_launch::AutoLaunch::new("TimeWise", &exe.to_string_lossy(), true, &[] as &[&str]);
        #[cfg(not(target_os = "macos"))]
        let auto = auto_launch::AutoLaunch::new("TimeWise", &exe.to_string_lossy(), &[] as &[&str]);
        match auto.enable() {
            Ok(()) => println!("[timewise] autostart enabled"),
            Err(e) => eprintln!("[timewise] autostart registration failed: {e}"),
        }
    }
}

fn start_worker(app: &AppHandle, shared: &Shared, cfg: Config) {
    // Restart-safe: stop a previously running worker runtime first.
    if let Some(tx) = shared.worker_shutdown.lock().take() {
        let _ = tx.send(true);
    }
    let conn = match worker_runtime::open_buffer(&shared.dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[timewise] buffer db open failed: {e}");
            return;
        }
    };
    enable_autostart();
    let (tx, rx) = tokio::sync::watch::channel(false);
    *shared.worker_shutdown.lock() = Some(tx);
    let rt = WorkerRuntime::new(cfg, conn, ActiveWinPosRs, TauriNotifier(app.clone()));
    tauri::async_runtime::spawn(rt.run(rx));
    println!("[timewise] worker runtime started");
}

fn machine_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| {
            // GUI apps on macOS often lack HOSTNAME in their environment.
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "unknown".into())
        })
}

fn os_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

#[tauri::command]
fn get_state(state: State<Shared>) -> UiState {
    let cfg = Config::load_from(&state.dir).unwrap_or_else(|_| Config::new_first_run());
    UiState {
        role: cfg.role.map(|r| match r {
            Role::Master => "master".into(),
            Role::Worker => "worker".into(),
        }),
        port: cfg.port,
        worker_id: cfg.worker_id,
        masters: cfg.masters,
        track_self: cfg.track_self,
        idle_threshold_s: cfg.idle_threshold_s,
    }
}

#[tauri::command]
fn set_role(app: AppHandle, state: State<Shared>, role: String) -> Result<(), String> {
    let mut cfg = Config::load_from(&state.dir).map_err(|e| e.to_string())?;
    cfg.role = Some(match role.as_str() {
        "master" => Role::Master,
        "worker" => Role::Worker,
        _ => return Err(format!("invalid role: {role}")),
    });
    cfg.save_to(&state.dir).map_err(|e| e.to_string())?;
    match cfg.role {
        Some(Role::Master) => start_master(&app, &state, cfg),
        Some(Role::Worker) => start_worker(&app, &state, cfg),
        None => {}
    }
    Ok(())
}

/// Master-only: toggle tracking of the parent's own account (iteration 2).
#[tauri::command]
fn set_track_self(app: AppHandle, state: State<Shared>, enabled: bool) -> Result<(), String> {
    let mut cfg = Config::load_from(&state.dir).map_err(|e| e.to_string())?;
    cfg.track_self = enabled;
    cfg.save_to(&state.dir).map_err(|e| e.to_string())?;
    if enabled {
        start_self_worker(&app, &state, &cfg);
    } else if let Some(tx) = state.worker_shutdown.lock().take() {
        let _ = tx.send(true); // stop the self-tracking worker
    }
    Ok(())
}

#[tauri::command]
async fn discover() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        timewise_app::pairing::discover_masters(std::time::Duration::from_secs(4))
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn pair_master(app: AppHandle, state: State<'_, Shared>, base_url: String) -> Result<String, String> {
    let mut cfg = Config::load_from(&state.dir).map_err(|e| e.to_string())?;
    if cfg.masters.iter().any(|m| m.base_url == base_url) {
        return Ok("already paired".into());
    }
    let token = uuid::Uuid::new_v4().to_string();
    let req = RegisterRequest {
        worker_id: cfg.worker_id.clone(),
        hostname: machine_hostname(),
        os: std::env::consts::OS.into(),
        os_user: os_username(),
        token: token.clone(),
    };
    let client = timewise_app::sync::ReqwestClient::new(&base_url);
    let resp = timewise_app::pairing::register_with_master(&client, &req)
        .await
        .map_err(|e| format!("cannot reach master at {base_url}: {e}"))?;
    cfg.masters.push(MasterRegistration { base_url, token });
    cfg.save_to(&state.dir).map_err(|e| e.to_string())?;
    if cfg.role == Some(Role::Worker) {
        start_worker(&app, &state, cfg);
    }
    Ok(format!("registered: {:?}", resp.status))
}

fn main() {
    let dir = Config::dir();
    let shared = Shared { dir: dir.clone(), worker_shutdown: Mutex::new(None) };
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![get_state, set_role, discover, pair_master, set_track_self])
        .setup(|app| {
            // System tray: the app minimizes to the tray instead of quitting.
            let show_item = MenuItemBuilder::with_id("show", "Show TimeWise").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit TimeWise").build(app)?;
            let menu = MenuBuilder::new(app).items(&[&show_item, &quit_item]).build()?;
            let mut tray = TrayIconBuilder::new().menu(&menu).tooltip("TimeWise");
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.on_menu_event(|app, event| match event.id().as_ref() {
                "show" => {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
                "quit" => app.exit(0),
                _ => {}
            })
            .build(app)?;

            // Close button hides to tray; real quit lives in the tray menu.
            if let Some(window) = app.get_webview_window("main") {
                let w = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            let state = app.state::<Shared>();
            let cfg = Config::load_from(&state.dir).unwrap_or_else(|_| Config::new_first_run());
            match cfg.role {
                Some(Role::Master) => start_master(&app.handle(), &state, cfg),
                Some(Role::Worker) => start_worker(&app.handle(), &state, cfg),
                None => {} // first-run role screen shown by the UI
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running TimeWise");
}
