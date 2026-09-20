//! The localhost API the cockpit relays through.
//!
//! Bound to 127.0.0.1 with a bearer token from the support directory. The token matters
//! even on loopback: any process on the machine — including a browser page — can reach
//! 127.0.0.1, and these endpoints start runs and write the vault.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use wikiskill_core::state::{PendingLabel, RunState, Schedule};

use crate::app::App;

/// Anyhow inside, JSON outside.
pub struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn not_found(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, message.into())
}

pub fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/config", get(config))
        .route("/v1/profile", get(profile))
        .route("/v1/runs", get(list_runs).post(create_run))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/pause", post(pause_run))
        .route("/v1/runs/{id}/resume", post(resume_run))
        .route("/v1/runs/{id}/cancel", post(cancel_run))
        .route("/v1/baseline", post(baseline))
        .route("/v1/skills", get(skills))
        .route("/v1/skills/{name}/history", get(skill_history))
        .route("/v1/skills/{name}/revert", post(revert_skill))
        .route("/v1/impact", get(impact))
        .route("/v1/patterns", get(patterns))
        .route("/v1/labels", get(labels).post(resolve_label))
        .route("/v1/schedules", get(schedules).put(set_schedules))
        .route("/v1/note", get(note))
        .route("/v1/capture", post(capture))
        .route("/v1/proposals", get(proposals))
        .route("/v1/proposals/{id}/accept", post(accept_proposal))
        .route("/v1/proposals/{id}/reject", post(reject_proposal))
        // Credentials are write-only over the API: the cockpit can set one and see whether
        // one exists, and there is no route that returns a value.
        .route("/v1/credentials", get(credentials))
        .route(
            "/v1/credentials/{service}",
            axum::routing::put(set_credential).delete(delete_credential),
        )
        .route("/v1/obsidian", get(obsidian_setup))
        .route("/v1/obsidian/prepare", post(obsidian_prepare))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&app),
            authorize,
        ));

    Router::new()
        // Unauthenticated on purpose: the cockpit needs to know whether the daemon is up
        // before it has read the token, and this leaks nothing.
        .route("/health", get(health))
        .merge(protected)
        .with_state(app)
}

async fn authorize(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), app.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "bad or missing bearer token" })),
        )
            .into_response();
    }
    next.run(request).await
}

/// Length-independent compare, so a wrong token cannot be found a byte at a time.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct Status {
    /// Which config file this daemon actually loaded — the first thing to check when the
    /// cockpit is showing a vault the user did not expect.
    config_path: String,
    vault: String,
    vault_link: String,
    state_dir: String,
    gate_rule: &'static str,
    iterations: u32,
    jev_enabled: bool,
    jev_shadow_mode: bool,
    executor_running: bool,
    executor_pinned: String,
    agent_file: String,
    active_run: Option<String>,
    active_skills: Vec<String>,
    planned_rollouts: Option<usize>,
    task_set_error: Option<String>,
}

async fn status(State(app): State<Arc<App>>) -> ApiResult<Json<Status>> {
    let (planned_rollouts, task_set_error) = match app.task_set().await {
        Ok(tasks) => (
            Some(tasks.planned_rollouts(
                app.config.r#loop.iterations,
                app.config.r#loop.gate_rule.validation_runs(),
            )),
            None,
        ),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    Ok(Json(Status {
        config_path: app.config_path.display().to_string(),
        vault: app.config.vault.display().to_string(),
        vault_link: app.vault.obsidian_link("wiki/index.md"),
        state_dir: app.config.daemon.state_dir.display().to_string(),
        gate_rule: app.config.r#loop.gate_rule.label(),
        iterations: app.config.r#loop.iterations,
        jev_enabled: app.config.jev.enabled,
        jev_shadow_mode: app.config.jev.shadow_mode,
        executor_running: app.supervisor.running().await,
        executor_pinned: app.config.executor.pinned_version.clone(),
        agent_file: crate::supervisor::agent_file_path(&app.config)
            .display()
            .to_string(),
        active_run: app.active_run().await?,
        active_skills: app
            .vault
            .active_skills()
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect(),
        planned_rollouts,
        task_set_error,
    }))
}

/// The config as the daemon sees it. `daemon.token` is stripped: the cockpit already has
/// the token it needs, and nothing else should be able to read it back out.
async fn config(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let mut value = serde_json::to_value(&app.config).map_err(anyhow::Error::from)?;
    if let Some(daemon) = value.get_mut("daemon").and_then(|d| d.as_object_mut()) {
        daemon.remove("token");
    }
    Ok(Json(value))
}

async fn profile(State(app): State<Arc<App>>) -> ApiResult<String> {
    Ok(app.render_profile()?)
}

async fn list_runs(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<RunState>>> {
    Ok(Json(app.store.list_runs().await?))
}

async fn get_run(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunState>> {
    app.store
        .load_run(&id)
        .await
        .map(Json)
        .map_err(|e| not_found(format!("{e:#}")))
}

#[derive(Deserialize, Default)]
struct CreateRun {
    /// Overrides `models.inference.id` for this run — the exploratory-model comparison.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
struct Created {
    id: String,
}

async fn create_run(
    State(app): State<Arc<App>>,
    body: Option<Json<CreateRun>>,
) -> ApiResult<Json<Created>> {
    let model = body.and_then(|Json(b)| b.model);
    let id = app
        .start_run(model)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(Created { id }))
}

async fn baseline(State(app): State<Arc<App>>) -> ApiResult<Json<RunState>> {
    Ok(Json(app.baseline().await?))
}

#[derive(Serialize)]
struct Ack {
    ok: bool,
    /// What the daemon will actually do, for the cockpit to show verbatim.
    detail: String,
}

async fn pause_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Ack>> {
    let control = app
        .control(&id)
        .await
        .ok_or_else(|| not_found(format!("run `{id}` is not active in this daemon")))?;
    control.pause();
    Ok(Json(Ack {
        ok: true,
        detail: "the run will stop at the next iteration boundary, so no proposal is \
                 left half-applied"
            .into(),
    }))
}

async fn resume_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Ack>> {
    let control = app
        .control(&id)
        .await
        .ok_or_else(|| not_found(format!("run `{id}` is not active in this daemon")))?;
    control.resume();
    Ok(Json(Ack {
        ok: true,
        detail: "resumed".into(),
    }))
}

async fn cancel_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Ack>> {
    let control = app
        .control(&id)
        .await
        .ok_or_else(|| not_found(format!("run `{id}` is not active in this daemon")))?;
    control.cancel();
    Ok(Json(Ack {
        ok: true,
        detail: "the run will stop after the current iteration".into(),
    }))
}

#[derive(Serialize)]
struct SkillView {
    name: String,
    purpose: Option<String>,
    body: String,
    obsidian_link: String,
}

async fn skills(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<SkillView>>> {
    Ok(Json(
        app.vault
            .active_skills()
            .await?
            .into_iter()
            .map(|skill| SkillView {
                obsidian_link: app
                    .vault
                    .obsidian_link(&format!("skills/{}/SKILL.md", skill.name)),
                name: skill.name,
                purpose: skill.purpose,
                body: skill.body,
            })
            .collect(),
    ))
}

#[derive(Serialize)]
struct HistoryEntry {
    commit: String,
    subject: String,
}

async fn skill_history(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<HistoryEntry>>> {
    Ok(Json(
        app.repo
            .skill_history(&name)
            .await?
            .into_iter()
            .map(|(commit, subject)| HistoryEntry { commit, subject })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct Revert {
    commit: String,
}

async fn revert_skill(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(body): Json<Revert>,
) -> ApiResult<Json<Ack>> {
    if app.active_run().await?.is_some() {
        return Err(bad_request(
            "a run is active; reverting a skill mid-run would change what the next \
             rollout sees. Pause the run first.",
        ));
    }
    app.repo.revert_skill_to(&name, &body.commit).await?;
    app.repo
        .commit_all(&format!(
            "revert skills/{name} to {} (from the cockpit)",
            &body.commit[..body.commit.len().min(12)]
        ))
        .await?;
    Ok(Json(Ack {
        ok: true,
        detail: format!("skills/{name} restored and committed"),
    }))
}

/// The impact log verbatim. The cockpit's Gate view renders it rather than reformatting:
/// what a reviewer sees should be what is committed.
async fn impact(State(app): State<Arc<App>>) -> ApiResult<String> {
    Ok(tokio::fs::read_to_string(app.vault.impact_log())
        .await
        .map_err(|e| anyhow::anyhow!("reading the impact log: {e}"))?)
}

#[derive(Serialize)]
struct PatternView {
    name: String,
    occurrences: u32,
    obsidian_link: String,
}

async fn patterns(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<PatternView>>> {
    let counts = app.store.load_pattern_counts().await?;
    Ok(Json(
        counts
            .into_iter()
            .map(|(name, occurrences)| PatternView {
                obsidian_link: app
                    .vault
                    .obsidian_link(&format!("wiki/patterns/{name}.md")),
                name,
                occurrences,
            })
            .collect(),
    ))
}

async fn labels(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<PendingLabel>>> {
    Ok(Json(app.store.load_pending_labels().await?))
}

#[derive(Deserialize)]
struct LabelVerdict {
    note_path: String,
    /// The human's pass/fail, which overrides Jev's low-confidence guess.
    verdict: bool,
}

async fn resolve_label(
    State(app): State<Arc<App>>,
    Json(body): Json<LabelVerdict>,
) -> ApiResult<Json<Ack>> {
    let mut pending = app.store.load_pending_labels().await?;
    let Some(entry) = pending.iter_mut().find(|l| l.note_path == body.note_path) else {
        return Err(not_found(format!("no pending label for {}", body.note_path)));
    };
    entry.human_verdict = Some(body.verdict);
    app.store.save_pending_labels(&pending).await?;
    Ok(Json(Ack {
        ok: true,
        detail: "recorded; the label stays with the note and is not rewritten into it, \
                 because raw notes are immutable"
            .into(),
    }))
}

async fn schedules(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<Schedule>>> {
    Ok(Json(app.store.load_schedules().await?))
}

async fn set_schedules(
    State(app): State<Arc<App>>,
    Json(body): Json<Vec<Schedule>>,
) -> ApiResult<Json<Ack>> {
    for schedule in &body {
        if schedule.hour > 23 {
            return Err(bad_request(format!(
                "schedule `{}` has hour {}; must be 0-23",
                schedule.id, schedule.hour
            )));
        }
        if schedule.weekday.is_some_and(|w| w > 6) {
            return Err(bad_request(format!(
                "schedule `{}` has weekday {:?}; must be 0-6 with 0 = Monday",
                schedule.id, schedule.weekday
            )));
        }
    }
    app.store.save_schedules(&body).await?;
    Ok(Json(Ack {
        ok: true,
        detail: "saved".into(),
    }))
}

#[derive(Deserialize)]
struct NoteQuery {
    /// Vault-relative path, e.g. `raw/iter-003/fix-flaky.md`.
    path: String,
}

/// Reads one vault file for the cockpit. Path resolution goes through the vault's own
/// checks, so `..` and absolute paths are refused rather than sanitised.
async fn note(
    State(app): State<Arc<App>>,
    Query(query): Query<NoteQuery>,
) -> ApiResult<String> {
    let (area, rest) = query
        .path
        .split_once('/')
        .ok_or_else(|| bad_request("path must start with an area, e.g. raw/…"))?;
    let area = match area {
        "raw" => wikiskill_core::vault::Area::Raw,
        "wiki" => wikiskill_core::vault::Area::Wiki,
        "skills" => wikiskill_core::vault::Area::Skills,
        "eval" => wikiskill_core::vault::Area::Eval,
        other => return Err(bad_request(format!("unknown vault area `{other}`"))),
    };
    let path = app
        .vault
        .resolve_in(area, rest)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| not_found(format!("reading {}: {e}", query.path)))
}

/// Where the OpenCode capture plugin posts a finished daily session.
async fn capture(
    State(app): State<Arc<App>>,
    Json(body): Json<crate::daily::CaptureRequest>,
) -> ApiResult<Json<crate::daily::CaptureResponse>> {
    Ok(Json(
        crate::daily::capture(&app, body)
            .await
            .map_err(|e| bad_request(format!("{e:#}")))?,
    ))
}

async fn proposals(
    State(app): State<Arc<App>>,
) -> ApiResult<Json<Vec<wikiskill_core::state::PendingProposal>>> {
    Ok(Json(app.store.load_pending_proposals().await?))
}

async fn accept_proposal(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Ack>> {
    if app.active_run().await?.is_some() {
        return Err(bad_request(
            "a run is active; accepting a proposal now would change what the next rollout \
             sees mid-run. Pause the run first.",
        ));
    }
    crate::daily::accept_proposal(&app, &id)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(Ack {
        ok: true,
        detail: format!("{id} applied to skills/ and committed"),
    }))
}

async fn reject_proposal(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Ack>> {
    crate::daily::reject_proposal(&app, &id)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(Ack {
        ok: true,
        detail: format!("{id} rejected; skills/ was already rolled back when it was captured"),
    }))
}

// ------------------------------------------------------------------ credentials
//
// The rationale's rule is that a credential never reaches the webview. Setting one from the
// cockpit does not weaken that: values travel inwards only. `GET` reports whether a key
// exists, never what it is, and there is no endpoint that reads one back.

async fn credentials(
    State(app): State<Arc<App>>,
) -> ApiResult<Json<Vec<crate::secrets::CredentialStatus>>> {
    Ok(Json(crate::secrets::status(app.config.jev.enabled).await?))
}

#[derive(Deserialize)]
struct SetCredential {
    value: String,
}

async fn set_credential(
    State(app): State<Arc<App>>,
    Path(service): Path<String>,
    Json(body): Json<SetCredential>,
) -> ApiResult<Json<Ack>> {
    let spec = crate::secrets::spec_for(&service)
        .ok_or_else(|| not_found(format!("`{service}` is not one of this daemon's credentials")))?;
    // Mid-run is the one time this is dangerous: rollouts already running would keep the old
    // key while the impact log records the run as one thing.
    if app.active_run().await?.is_some() {
        return Err(bad_request(
            "a run is active; changing a credential now would leave the run using the old key. \
             Pause the run first.",
        ));
    }
    crate::secrets::validate_value(&body.value).map_err(|e| bad_request(format!("{e:#}")))?;
    crate::secrets::write(&service, &body.value).await?;
    crate::secrets::remember_in_env(spec, &body.value);
    Ok(Json(Ack {
        ok: true,
        detail: format!(
            "stored in the Keychain as `{service}` and loaded into the daemon as {}. It is not \
             readable back through this API.",
            spec.env
        ),
    }))
}

async fn delete_credential(
    State(app): State<Arc<App>>,
    Path(service): Path<String>,
) -> ApiResult<Json<Ack>> {
    let spec = crate::secrets::spec_for(&service)
        .ok_or_else(|| not_found(format!("`{service}` is not one of this daemon's credentials")))?;
    if app.active_run().await?.is_some() {
        return Err(bad_request(
            "a run is active; removing a credential now would fail the run mid-iteration. \
             Pause the run first.",
        ));
    }
    let existed = crate::secrets::delete(&service).await?;
    crate::secrets::forget_in_env(spec);
    Ok(Json(Ack {
        ok: true,
        detail: if existed {
            format!("removed `{service}` from the Keychain and from the daemon's environment")
        } else {
            format!("`{service}` was not in the Keychain; cleared it from the daemon's environment")
        },
    }))
}

// ------------------------------------------------------------------ obsidian

async fn obsidian_setup(State(app): State<Arc<App>>) -> ApiResult<Json<crate::obsidian::Setup>> {
    Ok(Json(
        crate::obsidian::setup(
            &app.config.vault,
            app.vault.obsidian_link("wiki/index.md"),
        )
        .await?,
    ))
}

async fn obsidian_prepare(State(app): State<Arc<App>>) -> ApiResult<Json<Ack>> {
    let written = crate::obsidian::prepare(&app.config.vault).await?;
    Ok(Json(Ack {
        ok: true,
        detail: if written.is_empty() {
            "the vault's Obsidian settings were already in place; nothing was changed".into()
        } else {
            format!(
                "wrote .obsidian/app.json ({}). Existing settings were left alone.",
                written.join(", ")
            )
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn token_compare_is_length_safe() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    /// Sends one request through the real router.
    async fn call(
        app: &Arc<App>,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(value) => request
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(Arc::clone(app)).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app::test_app(dir.path()).await.unwrap();
        (dir, app)
    }

    /// Any local process can reach 127.0.0.1, including a page in a browser, so everything
    /// that can start a run or write the vault has to demand the token.
    #[tokio::test]
    async fn every_endpoint_but_health_needs_the_token() {
        let (_d, app) = app().await;

        let (status, _) = call(&app, "GET", "/health", None, None).await;
        assert_eq!(status, StatusCode::OK);

        for uri in [
            "/v1/status",
            "/v1/config",
            "/v1/runs",
            "/v1/proposals",
            "/v1/credentials",
            "/v1/obsidian",
        ] {
            let (status, _) = call(&app, "GET", uri, None, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} was unprotected");
            let (status, _) = call(&app, "GET", uri, Some("wrong-token"), None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} took a bad token");
            let (status, _) = call(&app, "GET", uri, Some(&app.token), None).await;
            assert_eq!(status, StatusCode::OK, "{uri} refused the real token");
        }
    }

    /// The webview must never be able to read the daemon's own bearer token back out of
    /// the config it renders.
    #[tokio::test]
    async fn the_config_endpoint_strips_the_token() {
        let (_d, app) = app().await;
        let (status, body) = call(&app, "GET", "/v1/config", Some(&app.token), None).await;
        assert_eq!(status, StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["daemon"]["bind"].is_string());
        assert!(
            value["daemon"].get("token").is_none(),
            "the token leaked into /v1/config: {body}"
        );
    }

    #[tokio::test]
    async fn unknown_runs_and_proposals_are_not_500s() {
        let (_d, app) = app().await;
        let (status, _) = call(&app, "GET", "/v1/runs/nope", Some(&app.token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(
            &app,
            "POST",
            "/v1/proposals/nope/accept",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("no pending proposal"), "{body}");
    }

    /// A pending proposal round-trips through the API: listed, then accepted, with the diff
    /// replayed onto `skills/`.
    #[tokio::test]
    async fn a_reviewed_proposal_is_applied_by_accepting_it() {
        let (_d, app) = app().await;
        // Produce a real diff the same way the weekly proposer does: write, diff, roll back.
        let skill = app.vault.skills_dir().join("read-before-edit");
        tokio::fs::create_dir_all(&skill).await.unwrap();
        tokio::fs::write(skill.join("SKILL.md"), "# Read before edit\n")
            .await
            .unwrap();
        let diff = app.repo.skills_diff().await.unwrap();
        app.repo.unstage_skills().await.unwrap();
        app.repo.restore_skills().await.unwrap();
        assert!(!skill.join("SKILL.md").exists());

        app.store
            .save_pending_proposals(&[wikiskill_core::state::PendingProposal {
                id: "proposal-1".into(),
                skill: Some("read-before-edit".into()),
                diff,
                summary: "read the file first".into(),
                source: "weekly-proposer".into(),
                created: chrono::Utc::now(),
                accepted: None,
            }])
            .await
            .unwrap();

        let (status, body) = call(&app, "GET", "/v1/proposals", Some(&app.token), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("proposal-1"), "{body}");

        let (status, _) = call(
            &app,
            "POST",
            "/v1/proposals/proposal-1/accept",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            tokio::fs::read_to_string(skill.join("SKILL.md")).await.unwrap(),
            "# Read before edit\n"
        );
        // Reviewing it twice must not apply it twice.
        let (status, body) = call(
            &app,
            "POST",
            "/v1/proposals/proposal-1/accept",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("already reviewed"), "{body}");
    }

    #[tokio::test]
    async fn a_schedule_with_an_impossible_hour_is_refused() {
        let (_d, app) = app().await;
        let mut schedules = app.store.load_schedules().await.unwrap();
        schedules[0].hour = 25;
        let (status, body) = call(
            &app,
            "PUT",
            "/v1/schedules",
            Some(&app.token),
            Some(serde_json::to_value(&schedules).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("must be 0-23"), "{body}");
    }

    /// The note endpoint is a read into the vault driven by a query string, so it is the
    /// one place a traversal would be easiest; it must refuse rather than sanitise.
    #[tokio::test]
    async fn the_note_endpoint_refuses_traversal_and_unknown_areas() {
        let (_d, app) = app().await;
        for path in ["wiki/../../etc/passwd", "etc/passwd", "/etc/passwd"] {
            let uri = format!("/v1/note?path={}", urlencoding(path));
            let (status, body) = call(&app, "GET", &uri, Some(&app.token), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path} → {body}");
        }
        let (status, body) = call(
            &app,
            "GET",
            "/v1/note?path=wiki%2Findex.md",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Wiki index"), "{body}");
    }

    /// The whole point of routing credentials through the daemon: the cockpit learns whether
    /// a key is set, and cannot learn what it is.
    #[tokio::test]
    async fn credentials_are_reported_without_their_values() {
        let (_d, app) = app().await;
        let (status, body) = call(&app, "GET", "/v1/credentials", Some(&app.token), None).await;
        assert_eq!(status, StatusCode::OK);
        let listed: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(listed.len(), crate::secrets::SPECS.len());
        for entry in &listed {
            assert!(entry["env"].is_string());
            assert!(entry["in_keychain"].is_boolean());
            assert!(
                entry.get("value").is_none(),
                "a credential value was serialised out: {entry}"
            );
        }
        // There is no route that reads one back, either.
        let (status, _) = call(
            &app,
            "GET",
            "/v1/credentials/wikiskill-fireworks",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    /// Both of these must fail *before* anything is written to the Keychain.
    #[tokio::test]
    async fn a_bad_credential_write_never_reaches_the_keychain() {
        let (_d, app) = app().await;

        // An unknown service would otherwise be a Keychain write under a caller-chosen name.
        let (status, body) = call(
            &app,
            "PUT",
            "/v1/credentials/wikiskill-not-a-credential",
            Some(&app.token),
            Some(serde_json::json!({ "value": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // The mistake an actual person makes: pasting the whole shell line.
        let (status, body) = call(
            &app,
            "PUT",
            "/v1/credentials/wikiskill-fireworks",
            Some(&app.token),
            Some(serde_json::json!({ "value": "export FIREWORKS_API_KEY=fw_abc\n" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("control character"), "{body}");
    }

    /// The vault is an Obsidian folder, but registering it is the user's step: the cockpit has
    /// to be able to tell that it has not happened yet.
    #[tokio::test]
    async fn obsidian_setup_reports_an_unregistered_vault_and_can_prepare_it() {
        let (_d, app) = app().await;
        let (status, body) = call(&app, "GET", "/v1/obsidian", Some(&app.token), None).await;
        assert_eq!(status, StatusCode::OK);
        let setup: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            setup["vault_path"].as_str().unwrap(),
            app.config.vault.display().to_string()
        );
        assert_eq!(setup["registered"], serde_json::json!(false));
        assert!(
            setup["open_link"].as_str().unwrap().starts_with("obsidian://"),
            "{body}"
        );
        if setup["installed"] == serde_json::json!(true) {
            // Installed but not registered is the case that needs instructions.
            assert!(
                !setup["steps"].as_array().unwrap().is_empty(),
                "an unregistered vault must come with the steps to fix it: {body}"
            );
        }

        let (status, body) = call(
            &app,
            "POST",
            "/v1/obsidian/prepare",
            Some(&app.token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let settings: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(app.config.vault.join(".obsidian/app.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(settings["useMarkdownLinks"], serde_json::json!(false));
    }

    fn urlencoding(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
}
