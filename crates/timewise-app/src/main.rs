#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! TimeWise desktop shell: one binary, role per OS account (master | worker),
//! selected on first run (US-01). Master spawns the embedded API + mDNS;
//! worker spawns the tracking runtime with OS-native notifications.

use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
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

fn start_master(dir: PathBuf, cfg: Config) {
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

fn enable_autostart() {
    if let Ok(exe) = std::env::current_exe() {
        // auto-launch's constructor is OS-dependent (macOS takes use_launch_agent).
        #[cfg(target_os = "macos")]
        let auto = auto_launch::AutoLaunch::new("TimeWise", &exe.to_string_lossy(), false, &[] as &[&str]);
        #[cfg(not(target_os = "macos"))]
        let auto = auto_launch::AutoLaunch::new("TimeWise", &exe.to_string_lossy(), &[] as &[&str]);
        if let Err(e) = auto.enable() {
            eprintln!("[timewise] autostart registration failed: {e}");
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
        .unwrap_or_else(|_| "unknown".into())
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
        Some(Role::Master) => start_master(state.dir.clone(), cfg),
        Some(Role::Worker) => start_worker(&app, &state, cfg),
        None => {}
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
        .invoke_handler(tauri::generate_handler![get_state, set_role, discover, pair_master])
        .setup(|app| {
            let state = app.state::<Shared>();
            let cfg = Config::load_from(&state.dir).unwrap_or_else(|_| Config::new_first_run());
            match cfg.role {
                Some(Role::Master) => start_master(state.dir.clone(), cfg),
                Some(Role::Worker) => start_worker(&app.handle(), &state, cfg),
                None => {} // first-run role screen shown by the UI
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running TimeWise");
}
