//! Master embedded REST API (application-design §5). axum 0.8.
//!
//! Worker-facing routes authenticate via `Authorization: Bearer <token>` +
//! `x-worker-id`. Dashboard routes are LAN-open (accepted risk, design §5).
//! The parking_lot mutex is never held across `.await` (BR12).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json, Router,
};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use timewise_core::store::master as store;
use timewise_core::{model::*, timeutil, Categorizer};
use tower_http::cors::CorsLayer;

pub const ONLINE_THRESHOLD_S: i64 = 90;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    pub tz_offset_s: i32,
    pub break_prompt_after_min: u64,
    pub online_threshold_s: i64,
}

impl AppState {
    pub fn new(conn: Connection, tz_offset_s: i32, break_prompt_after_min: u64) -> Self {
        AppState {
            db: Arc::new(Mutex::new(conn)),
            tz_offset_s,
            break_prompt_after_min,
            online_threshold_s: ONLINE_THRESHOLD_S,
        }
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/register", axum::routing::post(register))
        .route("/api/v1/register/status", axum::routing::get(register_status))
        .route("/api/v1/sessions/batch", axum::routing::post(post_batch))
        .route("/api/v1/heartbeat", axum::routing::post(heartbeat))
        .route("/api/v1/config", axum::routing::get(get_config))
        .route("/api/v1/dashboard/summary", axum::routing::get(dashboard_summary))
        .route("/api/v1/dashboard/child/{id}", axum::routing::get(dashboard_child))
        .route("/api/v1/dashboard/uncategorized/{id}", axum::routing::get(dashboard_uncategorized))
        .route("/api/v1/dashboard/apps/{id}", axum::routing::get(dashboard_apps))
        .route("/api/v1/workers", axum::routing::get(list_all_workers))
        .route("/api/v1/workers/{id}/approve", axum::routing::post(approve_worker))
        .route("/api/v1/workers/{id}/assign", axum::routing::post(assign_worker))
        .route("/api/v1/children", axum::routing::get(list_all_children))
        .route("/api/v1/children/{id}/goal", axum::routing::post(set_child_goal))
        .route("/api/v1/categories/override", axum::routing::post(set_category_override))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---- Auth ----

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<WorkerInfo, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let worker_id = headers
        .get("x-worker-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let db = state.db.lock();
    let valid = store::token_valid(&db, worker_id, token).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !valid {
        return Err(StatusCode::UNAUTHORIZED);
    }
    store::get_worker(&db, worker_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn require_approved(w: &WorkerInfo) -> Result<(), StatusCode> {
    if w.approved { Ok(()) } else { Err(StatusCode::FORBIDDEN) }
}

fn status_of(state: &AppState, w: &WorkerInfo) -> RegisterResponse {
    let child_name = w.child_id.as_ref().and_then(|cid| {
        let db = state.db.lock();
        store::list_children(&db)
            .ok()?
            .into_iter()
            .find(|c| &c.id == cid)
            .map(|c| c.name)
    });
    RegisterResponse {
        status: if w.approved { RegistrationStatus::Approved } else { RegistrationStatus::Pending },
        child_name,
    }
}

// ---- Worker-facing handlers ----

/// BR11: idempotent registration; re-register returns current status.
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, StatusCode> {
    let w = {
        let db = state.db.lock();
        store::upsert_worker(&db, &req, now_ts()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        store::get_worker(&db, &req.worker_id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    };
    Ok(Json(status_of(&state, &w)))
}

async fn register_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RegisterResponse>, StatusCode> {
    let w = authenticate(&state, &headers)?;
    Ok(Json(status_of(&state, &w)))
}

async fn post_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<BatchUpload>,
) -> Result<Json<BatchAccepted>, StatusCode> {
    let w = authenticate(&state, &headers)?;
    require_approved(&w)?;
    let db = state.db.lock();
    let overrides = store::list_overrides(&db).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let categorizer = Categorizer::from_bundled().with_overrides(&overrides);
    let accepted = store::insert_sessions(&db, &w.worker_id, &batch.sessions, &categorizer)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(BatchAccepted { accepted }))
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<HeartbeatResponse>, StatusCode> {
    let w = authenticate(&state, &headers)?;
    let now = now_ts();
    let db = state.db.lock();
    store::touch_heartbeat(&db, &w.worker_id, now).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(HeartbeatResponse { server_time: now }))
}

async fn get_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ConfigResponse>, StatusCode> {
    let w = authenticate(&state, &headers)?;
    require_approved(&w)?;
    let child_id = w.child_id.clone().ok_or(StatusCode::CONFLICT)?;
    let now = now_ts();
    let db = state.db.lock();
    let goal = store::get_goal(&db, &child_id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let overrides = store::list_overrides(&db).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Shared goal: usage and points are per child, summed across devices.
    let usage = store::usage_totals(&db, &child_id, now, state.tz_offset_s)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let points_balance =
        store::points_balance(&db, &child_id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ConfigResponse {
        rules: vec![], // worker carries the bundled rule set; master pushes overrides only (v1)
        overrides,
        goal,
        thresholds: Thresholds::default(),
        usage,
        break_prompt_after_min: state.break_prompt_after_min as u32,
        points_balance,
    }))
}

// ---- Dashboard-facing handlers ----

async fn dashboard_summary(
    State(state): State<AppState>,
) -> Result<Json<DashboardSummary>, StatusCode> {
    let now = now_ts();
    let db = state.db.lock();
    // Points are evaluated on dashboard load as well as hourly (freshness).
    crate::points_engine::evaluate_all(&db, now, state.tz_offset_s)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let children = store::child_summaries(&db, now, state.tz_offset_s, state.online_threshold_s)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let pending = store::pending_workers(&db).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(DashboardSummary { children, pending }))
}

#[derive(Debug, Deserialize)]
struct RangeQuery {
    from: Option<i64>,
    to: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ChildDetail {
    breakdown: Vec<AppBreakdown>,
    tod: TodDistribution,
    goal: GoalConfig,
    usage: UsageTotals,
    points_balance: i64,
    points_history: Vec<PointsEntry>,
}

async fn dashboard_child(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(range): Query<RangeQuery>,
) -> Result<Json<ChildDetail>, StatusCode> {
    let now = now_ts();
    let to = range.to.unwrap_or(now);
    let from = range.from.unwrap_or_else(|| timeutil::day_start(to, state.tz_offset_s));
    let db = state.db.lock();
    let err = |_| StatusCode::INTERNAL_SERVER_ERROR;
    Ok(Json(ChildDetail {
        breakdown: store::app_breakdown(&db, &id, from, to).map_err(err)?,
        tod: store::tod_distribution(&db, &id, from, to, state.tz_offset_s).map_err(err)?,
        goal: store::get_goal(&db, &id).map_err(err)?,
        usage: store::usage_totals(&db, &id, now, state.tz_offset_s).map_err(err)?,
        points_balance: store::points_balance(&db, &id).map_err(err)?,
        points_history: store::points_history(&db, &id).map_err(err)?,
    }))
}

#[derive(Debug, Serialize)]
struct UncategorizedApp {
    app_name: String,
    total_s: i64,
}

async fn dashboard_uncategorized(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<UncategorizedApp>>, StatusCode> {
    let db = state.db.lock();
    let apps = store::uncategorized_apps(&db, &id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(apps.into_iter().map(|(app_name, total_s)| UncategorizedApp { app_name, total_s }).collect()))
}

async fn dashboard_apps(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<store::AppCategoryRow>>, StatusCode> {
    let db = state.db.lock();
    store::apps_with_categories(&db, &id).map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Debug, Deserialize)]
struct ApproveBody {
    /// Child name: matched case-insensitively against existing children
    /// (that IS the merge operation), otherwise a new child is created.
    child_name: String,
}

async fn approve_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock();
    let child_id =
        store::find_or_create_child(&db, &body.child_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let n = store::assign_worker_to_child(&db, &id, &child_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if n == 0 { Err(StatusCode::NOT_FOUND) } else { Ok(StatusCode::NO_CONTENT) }
}

#[derive(Debug, Deserialize)]
struct AssignBody {
    child_id: String,
}

/// Move a worker to a different child (the merge/reassign operation).
async fn assign_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignBody>,
) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock();
    let n = store::assign_worker_to_child(&db, &id, &body.child_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if n == 0 { Err(StatusCode::NOT_FOUND) } else { Ok(StatusCode::NO_CONTENT) }
}

async fn list_all_workers(State(state): State<AppState>) -> Result<Json<Vec<WorkerInfo>>, StatusCode> {
    let db = state.db.lock();
    store::list_workers(&db).map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_all_children(State(state): State<AppState>) -> Result<Json<Vec<ChildInfo>>, StatusCode> {
    let db = state.db.lock();
    store::list_children(&db).map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn set_child_goal(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GoalConfig>,
) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock();
    store::set_goal(&db, &id, body.daily_min, body.weekly_min)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_category_override(
    State(state): State<AppState>,
    Json(body): Json<CategoryOverride>,
) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock();
    store::set_override(&db, &body.app_name, body.category)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    store::recategorize_sessions(&db, &body.app_name, body.category)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as SC};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let conn = Connection::open_in_memory().unwrap();
        store::migrate(&conn).unwrap();
        AppState::new(conn, 0, 40)
    }

    fn json_req(method: &str, uri: &str, body: Option<String>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        b.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap()
    }

    fn authed(req: Request<Body>, worker_id: &str, token: &str) -> Request<Body> {
        let (mut parts, body) = req.into_parts();
        parts.headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        parts.headers.insert("x-worker-id", worker_id.parse().unwrap());
        Request::from_parts(parts, body)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn register_body(id: &str) -> String {
        serde_json::to_string(&RegisterRequest {
            worker_id: id.into(),
            hostname: "kid-pc".into(),
            os: "macos".into(),
            os_user: "ada".into(),
            token: "tok-1".into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn full_pairing_flow() {
        let app = router(test_state());
        // 1. Register -> pending.
        let resp = app.clone().oneshot(json_req("POST", "/api/v1/register", Some(register_body("w1")))).await.unwrap();
        assert_eq!(resp.status(), SC::OK);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "pending");
        // 2. Status poll -> pending.
        let resp = app.clone().oneshot(authed(json_req("GET", "/api/v1/register/status", None), "w1", "tok-1")).await.unwrap();
        assert_eq!(body_json(resp).await["status"], "pending");
        // 3. Upload before approval -> 403 (BR8 server-side enforcement).
        let batch = serde_json::to_string(&BatchUpload { sessions: vec![] }).unwrap();
        let resp = app.clone().oneshot(authed(json_req("POST", "/api/v1/sessions/batch", Some(batch)), "w1", "tok-1")).await.unwrap();
        assert_eq!(resp.status(), SC::FORBIDDEN);
        // 4. Approve (by name -> creates the child).
        let resp = app.clone().oneshot(json_req("POST", "/api/v1/workers/w1/approve", Some(r#"{"child_name":"Ada"}"#.into()))).await.unwrap();
        assert_eq!(resp.status(), SC::NO_CONTENT);
        // 5. Status poll -> approved with name.
        let resp = app.clone().oneshot(authed(json_req("GET", "/api/v1/register/status", None), "w1", "tok-1")).await.unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["status"], "approved");
        assert_eq!(v["child_name"], "Ada");
        // 6. Re-register keeps approval (BR11).
        let resp = app.clone().oneshot(json_req("POST", "/api/v1/register", Some(register_body("w1")))).await.unwrap();
        assert_eq!(body_json(resp).await["status"], "approved");
    }

    #[tokio::test]
    async fn bad_token_rejected() {
        let app = router(test_state());
        app.clone().oneshot(json_req("POST", "/api/v1/register", Some(register_body("w1")))).await.unwrap();
        let resp = app.clone().oneshot(authed(json_req("GET", "/api/v1/config", None), "w1", "wrong")).await.unwrap();
        assert_eq!(resp.status(), SC::UNAUTHORIZED);
        let resp = app.clone().oneshot(json_req("GET", "/api/v1/config", None)).await.unwrap();
        assert_eq!(resp.status(), SC::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn batch_upload_idempotent_and_config_reflects_goal() {
        let state = test_state();
        let app = router(state);
        app.clone().oneshot(json_req("POST", "/api/v1/register", Some(register_body("w1")))).await.unwrap();
        app.clone().oneshot(json_req("POST", "/api/v1/workers/w1/approve", Some(r#"{"child_name":"Ada"}"#.into()))).await.unwrap();
        // Goal path takes the CHILD id now; resolve it.
        let children_resp = app.clone().oneshot(json_req("GET", "/api/v1/children", None)).await.unwrap();
        let children = body_json(children_resp).await;
        let child_id = children[0]["id"].as_str().unwrap().to_string();
        app.clone().oneshot(json_req("POST", &format!("/api/v1/children/{child_id}/goal"), Some(r#"{"daily_min":120,"weekly_min":600}"#.into()))).await.unwrap();

        let batch = serde_json::to_string(&BatchUpload {
            sessions: vec![SessionRecord {
                id: "s1".into(),
                app_name: "Roblox".into(),
                window_title: "Roblox".into(),
                category: Category::Games,
                start_ts: 100,
                end_ts: 160,
                duration_s: 60,
            }],
        })
        .unwrap();
        let resp = app.clone().oneshot(authed(json_req("POST", "/api/v1/sessions/batch", Some(batch.clone())), "w1", "tok-1")).await.unwrap();
        assert_eq!(body_json(resp).await["accepted"], 1);
        // Retry: idempotent.
        let resp = app.clone().oneshot(authed(json_req("POST", "/api/v1/sessions/batch", Some(batch)), "w1", "tok-1")).await.unwrap();
        assert_eq!(body_json(resp).await["accepted"], 0);
        // Config carries goal + thresholds + points.
        let resp = app.clone().oneshot(authed(json_req("GET", "/api/v1/config", None), "w1", "tok-1")).await.unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["goal"]["daily_min"], 120);
        assert_eq!(v["thresholds"]["nudge_pct"], 90);
        assert_eq!(v["break_prompt_after_min"], 40);
        assert_eq!(v["points_balance"], 0);
    }

    #[tokio::test]
    async fn heartbeat_flips_online_status() {
        let app = router(test_state());
        app.clone().oneshot(json_req("POST", "/api/v1/register", Some(register_body("w1")))).await.unwrap();
        app.clone().oneshot(json_req("POST", "/api/v1/workers/w1/approve", Some(r#"{"child_name":"Ada"}"#.into()))).await.unwrap();
        // Offline before any heartbeat.
        let resp = app.clone().oneshot(json_req("GET", "/api/v1/dashboard/summary", None)).await.unwrap();
        assert_eq!(body_json(resp).await["children"][0]["online"], false);
        // Heartbeat -> online.
        let resp = app.clone().oneshot(authed(json_req("POST", "/api/v1/heartbeat", None), "w1", "tok-1")).await.unwrap();
        assert!(body_json(resp).await["server_time"].as_i64().unwrap() > 0);
        let resp = app.clone().oneshot(json_req("GET", "/api/v1/dashboard/summary", None)).await.unwrap();
        assert_eq!(body_json(resp).await["children"][0]["online"], true);
    }

    #[tokio::test]
    async fn approve_unknown_worker_404() {
        let app = router(test_state());
        let resp = app.clone().oneshot(json_req("POST", "/api/v1/workers/ghost/approve", Some(r#"{"child_name":"X"}"#.into()))).await.unwrap();
        assert_eq!(resp.status(), SC::NOT_FOUND);
    }
}
