use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tower_http::{catch_panic::CatchPanicLayer, limit::RequestBodyLimitLayer};

use crate::{
    DEFAULT_API_URL,
    config::ClientConfig,
    pipeline::{ContributorDetails, Runtime},
    state::PublicRunStatus,
};

#[derive(Clone)]
struct UiState {
    runtime: Runtime,
    token: String,
    html: Arc<String>,
}

pub async fn serve(runtime: Runtime) -> anyhow::Result<()> {
    let mut token_bytes = [0_u8; 24];
    rand::rng().fill_bytes(&mut token_bytes);
    let token = hex::encode(token_bytes);
    let html = Arc::new(INDEX_HTML.to_owned());
    let state = UiState {
        runtime,
        token: token.clone(),
        html,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(config))
        .route("/api/resumable", get(resumable))
        .route("/api/pick-folder", post(pick_folder))
        .route("/api/register", post(register))
        .route("/api/upload", post(upload))
        .route("/api/status/{run_id}", get(status))
        .route("/api/report/{run_id}", get(report))
        .route("/api/resume/{run_id}", post(resume))
        .route("/api/reprepare/{run_id}", post(reprepare))
        .layer(middleware::from_fn(require_loopback_host))
        .layer(RequestBodyLimitLayer::new(32 * 1024))
        .layer(CatchPanicLayer::new())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    // Keep the bearer token in the URL fragment: fragments are not sent in
    // HTTP requests, and the page removes it from browser history immediately.
    // A local process that discovers the loopback port can fetch the shell but
    // cannot invoke any file-selection or upload API.
    let url = format!("http://{address}/#{token}");
    println!("Scaling Neuro is ready at {url}");
    if webbrowser::open(&url).is_err() {
        println!("Open that address in a browser to continue.");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn require_loopback_host(request: axum::extract::Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| {
            host == "127.0.0.1"
                || host.starts_with("127.0.0.1:")
                || host == "localhost"
                || host.starts_with("localhost:")
        });
    if allowed {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

async fn index(State(state): State<UiState>) -> Html<String> {
    Html((*state.html).clone())
}

async fn config(
    State(state): State<UiState>,
    headers: HeaderMap,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    match ClientConfig::load(&state.runtime.paths) {
        Ok(config) => Ok(Json(json!({
            "enrolled": true,
            "project_id": config.project_id,
            "project_name": config.project_name,
            "consent_policy_version": config.consent_policy_version,
            "api_url": config.api_url,
        }))),
        Err(_) => {
            let contribution = state
                .runtime
                .contribution_info(DEFAULT_API_URL)
                .await
                .map_err(UiError::internal)?;
            Ok(Json(json!({
                "enrolled": false,
                "api_url": DEFAULT_API_URL,
                "registration_open": contribution.registration_open,
                "project_name": contribution.project_name,
                "consent_policy_version": contribution.consent_policy_version,
                "policy_url": contribution.policy_url,
                "self_service_quota_bytes": contribution.self_service_quota_bytes,
            })))
        }
    }
}

async fn resumable(
    State(state): State<UiState>,
    headers: HeaderMap,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let mut runs = state
        .runtime
        .state
        .in_progress_runs()
        .map_err(UiError::internal)?
        .into_iter()
        .filter(|run| state.runtime.is_run_active(&run.id))
        .collect::<Vec<_>>();
    let active_ids = runs
        .iter()
        .map(|run| run.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let resumable = state
        .runtime
        .state
        .resumable_runs(None)
        .map_err(UiError::internal)?;
    runs.extend(
        resumable
            .into_iter()
            .filter(|run| !active_ids.contains(&run.id)),
    );
    let active = runs
        .first()
        .is_some_and(|run| state.runtime.is_run_active(&run.id));
    let requires_reprepare = !active
        && runs
            .first()
            .is_some_and(|run| state.runtime.run_requires_privacy_repreparation(run));
    let next_run = runs.first().map(PublicRunStatus::from);
    Ok(Json(json!({
        "next_run": next_run,
        "pending_count": runs.len(),
        "active": active,
        "requires_reprepare": requires_reprepare,
    })))
}

async fn pick_folder(
    State(state): State<UiState>,
    headers: HeaderMap,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let path = native_folder_selection()
        .await
        .map_err(UiError::internal)?
        .ok_or_else(|| UiError::bad_request("folder_selection_cancelled"))?;
    Ok(Json(json!({ "path": path.to_string_lossy() })))
}

#[cfg(target_os = "macos")]
const MACOS_FOLDER_PICKER_SCRIPT: &str = r#"try
  set selectedFolder to choose folder with prompt "Select the completed DICOM session folder"
  return POSIX path of selectedFolder
on error number -128
  return "__NEURO_SYNC_FOLDER_CANCELLED__"
end try"#;

#[cfg(target_os = "macos")]
async fn native_folder_selection() -> anyhow::Result<Option<PathBuf>> {
    let output = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-e", MACOS_FOLDER_PICKER_SCRIPT])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|_| anyhow::anyhow!("native_folder_picker_launch_failed"))?;
    if !output.status.success() || output.stdout.len() > 32 * 1024 {
        anyhow::bail!("native_folder_picker_failed");
    }
    parse_macos_folder_picker_output(&output.stdout)
}

#[cfg(target_os = "macos")]
fn parse_macos_folder_picker_output(bytes: &[u8]) -> anyhow::Result<Option<PathBuf>> {
    let mut value = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("native_folder_picker_returned_invalid_text"))?;
    if let Some(without_newline) = value.strip_suffix('\n') {
        value = without_newline;
    }
    if let Some(without_return) = value.strip_suffix('\r') {
        value = without_return;
    }
    if value == "__NEURO_SYNC_FOLDER_CANCELLED__" {
        return Ok(None);
    }
    if value.is_empty() || value.contains('\0') {
        anyhow::bail!("native_folder_picker_returned_invalid_path");
    }
    Ok(Some(PathBuf::from(value)))
}

#[cfg(not(target_os = "macos"))]
async fn native_folder_selection() -> anyhow::Result<Option<PathBuf>> {
    Ok(rfd::AsyncFileDialog::new()
        .set_title("Select the completed DICOM session folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf()))
}

#[derive(Deserialize)]
struct RegisterBody {
    contact_email: String,
    contact_name: String,
    institution_name: String,
    #[serde(default)]
    institution_ror_id: Option<String>,
    lab_name: String,
    #[serde(default)]
    contact_opt_in: bool,
    accepted_consent_policy_version: String,
}

async fn register(
    State(state): State<UiState>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let config = state
        .runtime
        .register(
            ContributorDetails {
                contact_email: body.contact_email,
                contact_name: body.contact_name,
                institution_name: body.institution_name,
                institution_ror_id: body.institution_ror_id,
                lab_name: body.lab_name,
                contact_opt_in: body.contact_opt_in,
            },
            body.accepted_consent_policy_version,
            DEFAULT_API_URL,
            format!("{} workstation", std::env::consts::OS),
        )
        .await
        .map_err(UiError::registration)?;
    Ok(Json(json!({
        "project_id": config.project_id,
        "project_name": config.project_name,
        "consent_policy_version": config.consent_policy_version,
    })))
}

#[derive(Deserialize)]
struct UploadBody {
    path: String,
    #[serde(default)]
    dry_run: bool,
    approval_confirmed: bool,
}

async fn upload(
    State(state): State<UiState>,
    headers: HeaderMap,
    Json(body): Json<UploadBody>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    if !body.approval_confirmed {
        return Err(UiError::bad_request(
            "project_approval_attestation_required",
        ));
    }
    let run_id = state
        .runtime
        .upload_in_background(PathBuf::from(body.path), body.dry_run)
        .await
        .map_err(UiError::internal)?;
    Ok(Json(json!({ "run_id": run_id })))
}

async fn status(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let run = state
        .runtime
        .run_record(Some(&run_id))
        .map_err(UiError::internal)?
        .ok_or_else(|| UiError::not_found("run_not_found"))?;
    Ok(Json(
        serde_json::to_value(PublicRunStatus::from(&run)).map_err(UiError::internal)?,
    ))
}

async fn report(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let report = state
        .runtime
        .report(Some(&run_id))
        .map_err(UiError::internal)?;
    // The UI needs counts and bundle sizes, never local object paths.
    let bytes: u64 = report
        .bundles
        .iter()
        .map(|bundle| bundle.nifti.size + bundle.metadata.size)
        .sum();
    Ok(Json(json!({
        "run_id": report.run_id,
        "status": report.status,
        "project_name": report.project_name,
        "source_summary": report.source_summary,
        "bytes_prepared": bytes,
        "bundle_count": report.bundles.len(),
        "existing_bundle_count": report.existing_bundles.len(),
        "archive_commit_count": report.archive_commit_count,
        "held_series": report.held_series,
        "errors": report.errors,
        "worker_upload_id": report.worker_upload_id,
    })))
}

async fn resume(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let run = state
        .runtime
        .state
        .resumable_runs(Some(&run_id))
        .map_err(UiError::internal)?
        .into_iter()
        .next()
        .ok_or_else(|| UiError::bad_request("run_not_resumable"))?;
    if state.runtime.run_requires_privacy_repreparation(&run) {
        return Err(UiError::bad_request("run_requires_privacy_repreparation"));
    }
    let started = state
        .runtime
        .resume_in_background(run_id.clone(), run.summary)
        .map_err(UiError::internal)?;
    if !started {
        return Err(UiError::bad_request("run_already_active"));
    }
    Ok(Json(json!({ "resuming": run_id })))
}

async fn reprepare(
    State(state): State<UiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> UiResult<Json<serde_json::Value>> {
    authorize(&state, &headers)?;
    let replacement = state
        .runtime
        .reprepare_in_background(&run_id)
        .map_err(UiError::internal)?
        .ok_or_else(|| UiError::bad_request("another_run_is_active"))?;
    Ok(Json(json!({
        "superseded_run_id": run_id,
        "run_id": replacement,
    })))
}

fn authorize(state: &UiState, headers: &HeaderMap) -> UiResult<()> {
    let supplied = headers
        .get("x-neuro-sync-token")
        .and_then(|value| value.to_str().ok());
    if supplied == Some(state.token.as_str()) {
        Ok(())
    } else {
        Err(UiError(
            StatusCode::FORBIDDEN,
            "invalid_local_ui_token".into(),
        ))
    }
}

type UiResult<T> = Result<T, UiError>;

struct UiError(StatusCode, String);

impl UiError {
    fn bad_request(code: &str) -> Self {
        Self(StatusCode::BAD_REQUEST, code.into())
    }

    fn not_found(code: &str) -> Self {
        Self(StatusCode::NOT_FOUND, code.into())
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "local UI operation failed");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "operation_failed".into())
    }

    fn registration(error: anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<crate::api::ApiFailure>() {
            let status = StatusCode::from_u16(failure.status).unwrap_or(StatusCode::BAD_REQUEST);
            let code = match failure.code.as_str() {
                "RATE_LIMITED" => "registration_rate_limited",
                "CLIENT_UPDATE_REQUIRED" => "client_update_required",
                "CONSENT_POLICY_UPDATE_REQUIRED" => "policy_update_required",
                "CONFLICT" => "email_or_registration_already_used",
                _ => "registration_failed",
            };
            tracing::warn!(code = %failure.code, "public registration failed");
            return Self(status, code.into());
        }
        tracing::warn!(error = %error, "public registration validation failed");
        Self(
            StatusCode::BAD_REQUEST,
            "registration_details_invalid".into(),
        )
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": { "code": self.1 } }))).into_response()
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Scaling Neuro · neuro-sync</title>
<link rel="icon" href="data:image/svg+xml,&lt;svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'&gt;&lt;text y='.9em' font-size='90'&gt;🧠&lt;/text&gt;&lt;/svg&gt;">
<style>
:root{color-scheme:light;--ink:#18233f;--muted:#65708a;--paper:#f6f5f9;--card:#fff;--line:#dcddea;--violet:#6656a5;--sage:#4f7d70;--coral:#c56f63}*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.5 ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}.shell{max-width:820px;margin:0 auto;padding:48px 24px 80px}header{display:flex;justify-content:space-between;align-items:center;margin-bottom:28px}.brand{font:600 20px Georgia,serif;letter-spacing:.02em}.pill{font-size:12px;padding:5px 10px;border:1px solid var(--line);border-radius:99px;color:var(--muted);background:#fff}.card{background:var(--card);border:1px solid var(--line);border-radius:18px;padding:24px;margin:16px 0;box-shadow:0 12px 40px rgba(24,35,63,.05)}h1{font:500 36px/1.1 Georgia,serif;margin:0 0 10px}h2{font-size:16px;margin:0 0 14px}p{color:var(--muted);margin:6px 0 16px}.project{display:grid;grid-template-columns:1fr auto;gap:12px;padding:13px 15px;border-radius:10px;background:#f2f1f7}.project strong,.project small{display:block}.project small{color:var(--muted)}button{appearance:none;border:0;border-radius:10px;padding:12px 17px;font-weight:650;cursor:pointer;background:var(--violet);color:#fff}button.secondary{background:#edf0f6;color:var(--ink)}button:disabled{opacity:.45;cursor:not-allowed}.folder{display:flex;align-items:center;gap:12px}.folder-path{flex:1;padding:11px 13px;background:#f7f7fa;border:1px solid var(--line);border-radius:9px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;color:var(--muted)}label.confirm{display:flex;gap:10px;margin:18px 0;color:var(--ink)}label.field{display:block;font-size:13px;font-weight:600}.registration-grid{display:grid;grid-template-columns:1fr 1fr;gap:0 14px}.registration-grid .wide{grid-column:1/-1}input[type=checkbox]{width:18px;height:18px;accent-color:var(--violet)}input[type=text],input[type=email]{width:100%;padding:12px;border:1px solid var(--line);border-radius:9px;margin:5px 0 10px;font:inherit}input:focus{outline:2px solid color-mix(in srgb,var(--violet) 35%,transparent);border-color:var(--violet)}.actions{display:flex;gap:10px}.hidden{display:none!important}.progress{height:8px;background:#ececf2;border-radius:9px;overflow:hidden;margin:18px 0}.progress i{display:block;height:100%;width:20%;background:linear-gradient(90deg,var(--violet),#897bc4);animation:pulse 1.5s infinite alternate}@keyframes pulse{to{transform:translateX(300%)}}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:9px}.metric{padding:12px;background:#f7f7fa;border-radius:9px}.metric b{display:block;font-size:21px}.metric span{font-size:11px;color:var(--muted);text-transform:uppercase;letter-spacing:.06em}.ok{color:var(--sage)}.held{color:var(--coral)}.fine{font-size:12px;color:var(--muted);margin-top:18px}@media(max-width:600px){.shell{padding:28px 15px}.folder{align-items:stretch;flex-direction:column}.metrics,.registration-grid{grid-template-columns:1fr}.registration-grid .wide{grid-column:auto}h1{font-size:30px}}
</style></head><body><main class="shell"><header><div class="brand">Scaling Neuro</div><div class="pill">private local client</div></header>
<section><h1>Share a completed EPI session.</h1><p>Set up once, then choose a DICOM folder. neuro-sync identifies functional EPI, converts in native space, preserves approved acquisition metadata, and holds everything uncertain on this machine.</p></section>
<section class="card hidden" id="register"><h2>Tell us which lab is contributing</h2><p>This one-time setup creates a private upload identity for your lab. It takes about a minute.</p><form id="registerForm"><div class="registration-grid"><label class="field">Your name<input id="contactName" type="text" autocomplete="name" maxlength="96" required></label><label class="field">Work email<input id="contactEmail" type="email" autocomplete="email" maxlength="254" required></label><label class="field">Institution<input id="institution" type="text" autocomplete="organization" maxlength="160" required></label><label class="field">Lab or research group<input id="labName" type="text" maxlength="160" required></label><label class="field wide">ROR ID <span class="fine">(optional)</span><input id="rorId" type="text" autocomplete="off" placeholder="https://ror.org/…" maxlength="25"></label></div><label class="confirm"><input type="checkbox" id="policyConfirm" required><span>I confirm these scans are approved for research contribution and accept the <a id="policyLink" href="https://scalingneuro.com/docs/contribution-policy" target="_blank" rel="noopener">EPI contribution policy</a>.</span></label><label class="confirm"><input type="checkbox" id="contactOptIn"><span>Scaling Neuro may contact me about this contribution or research collaboration.</span></label><button id="registerBtn" type="submit">Continue</button><p class="held" id="setupError" role="alert"></p></form><p class="fine">Your contact email is encrypted at rest. Participant identifiers are not collected by this form.</p></section>
<section class="card hidden" id="projectCard"><h2>Contribution destination</h2><div class="project"><div><strong id="project">Loading…</strong><small id="policy"></small></div><span class="pill" id="enrollState">checking</span></div></section>
<section class="card hidden" id="chooseCard"><h2>1 · Choose the DICOM folder</h2><div class="folder"><div class="folder-path" id="folderPath">No folder selected</div><button class="secondary" id="pickBtn">Choose folder…</button></div><label class="confirm"><input type="checkbox" id="approval"><span>I attest that these scans are approved for contribution under the project policy shown above.</span></label><div class="actions"><button id="uploadBtn" disabled>Validate and upload</button><button class="secondary" id="dryBtn" disabled>Local dry run</button></div><div class="fine">This attestation does not collect or substitute for participant consent. Source DICOMs are never modified or uploaded. Structural, diffusion, ASL, field-map, localizer, derived, and ambiguous series stay local.</div></section>
<section class="card hidden" id="progressCard"><h2 id="stage">Preparing…</h2><div class="progress" id="progress"><i></i></div><div class="metrics"><div class="metric"><b id="dicoms">0</b><span>DICOM files</span></div><div class="metric"><b class="ok" id="accepted">0</b><span>accepted</span></div><div class="metric"><b class="held" id="held">0</b><span>held</span></div><div class="metric"><b id="excluded">0</b><span>excluded</span></div></div><p id="result"></p><button class="secondary hidden" id="resumeBtn">Resume upload</button></section>
</main><script>
const fragmentToken=location.hash.slice(1),tokenPattern=/^[a-f0-9]{48}$/;if(tokenPattern.test(fragmentToken))sessionStorage.setItem('neuro-sync-token',fragmentToken);const token=tokenPattern.test(fragmentToken)?fragmentToken:(sessionStorage.getItem('neuro-sync-token')||''),validToken=tokenPattern.test(token);history.replaceState(null,'',location.pathname);let folder=null,runId=null,pollTimer=null,resumeMode='resume';
async function api(path,options={}){options.headers={...(options.headers||{}),'x-neuro-sync-token':token};if(options.body)options.headers['content-type']='application/json';const r=await fetch(path,options);const j=await r.json().catch(()=>({}));if(!r.ok)throw new Error(j?.error?.code||'operation_failed');return j}
const $=id=>document.getElementById(id);function err(e){$('result').textContent='Could not continue: '+e.message.replaceAll('_',' ')}
async function load(){if(!validToken){$('project').textContent='Open neuro-sync from the desktop app';$('enrollState').textContent='locked';$('chooseCard').classList.add('hidden');return}try{const c=await api('/api/config');$('register').classList.toggle('hidden',c.enrolled);$('chooseCard').classList.toggle('hidden',!c.enrolled);$('projectCard').classList.toggle('hidden',!c.enrolled);if(c.enrolled){$('project').textContent=c.project_name;$('policy').textContent='Private lab project · contribution policy '+c.consent_policy_version;$('enrollState').textContent='ready';await discoverResumable()}else{window.policyVersion=c.consent_policy_version;$('policyLink').href=c.policy_url;$('registerBtn').disabled=!c.registration_open;if(!c.registration_open)$('registerBtn').textContent='Registration temporarily paused'}}catch(e){$('register').classList.remove('hidden');$('registerBtn').disabled=true;$('setupError').textContent='Could not reach Scaling Neuro. Check the network and reopen neuro-sync.'}}
async function discoverResumable(){const pending=await api('/api/resumable');const s=pending.next_run;if(!s)return false;runId=s.id;folder=null;resumeMode=pending.requires_reprepare?'reprepare':'resume';$('folderPath').textContent=pending.active?'An upload is already running':pending.requires_reprepare?'The original private DICOM folder will be revalidated locally':'Resume the interrupted upload before choosing another folder';$('progressCard').classList.remove('hidden');$('pickBtn').disabled=true;$('approval').disabled=true;$('uploadBtn').disabled=true;$('dryBtn').disabled=true;$('dicoms').textContent=s.summary.dicom_files;$('accepted').textContent=s.summary.accepted;$('held').textContent=s.summary.held;$('excluded').textContent=s.summary.excluded;if(pending.active){$('progress').classList.remove('hidden');$('resumeBtn').classList.add('hidden');$('stage').textContent=labels[s.status]||s.status;$('result').textContent='This upload is still running in the local client.';poll()}else{$('progress').classList.add('hidden');$('resumeBtn').classList.remove('hidden');if(pending.requires_reprepare){$('resumeBtn').textContent='Revalidate with current privacy rules';$('stage').textContent='Privacy update required';$('result').textContent='This checkpoint was prepared by an older privacy contract. Revalidate the same private source locally before anything is uploaded.'}else{$('resumeBtn').textContent='Resume upload';$('stage').textContent='Interrupted upload ready to resume';const more=pending.pending_count>1?` ${pending.pending_count} interrupted uploads are queued and will be offered in order.`:'';$('result').textContent='Your prepared files and completed parts are checkpointed locally. Resume to continue without reconverting.'+more}}return true}
$('registerForm').onsubmit=async event=>{event.preventDefault();$('registerBtn').disabled=true;$('setupError').textContent='';try{await api('/api/register',{method:'POST',body:JSON.stringify({contact_email:$('contactEmail').value,contact_name:$('contactName').value,institution_name:$('institution').value,institution_ror_id:$('rorId').value||null,lab_name:$('labName').value,contact_opt_in:$('contactOptIn').checked,accepted_consent_policy_version:window.policyVersion})});await load()}catch(e){$('registerBtn').disabled=false;$('setupError').textContent=e.message.replaceAll('_',' ')}};
$('pickBtn').onclick=async()=>{try{const r=await api('/api/pick-folder',{method:'POST'});folder=r.path;$('folderPath').textContent=folder;buttons()}catch(e){if(e.message!=='folder_selection_cancelled')err(e)}};$('approval').onchange=buttons;function buttons(){const ready=!!folder&&$('approval').checked;$('uploadBtn').disabled=!ready;$('dryBtn').disabled=!ready}
async function start(dry){$('uploadBtn').disabled=true;$('dryBtn').disabled=true;$('pickBtn').disabled=true;$('approval').disabled=true;try{$('progressCard').classList.remove('hidden');$('result').textContent='';const r=await api('/api/upload',{method:'POST',body:JSON.stringify({path:folder,dry_run:dry,approval_confirmed:$('approval').checked})});runId=r.run_id;poll()}catch(e){$('pickBtn').disabled=false;$('approval').disabled=false;buttons();err(e)}}$('uploadBtn').onclick=()=>start(false);$('dryBtn').onclick=()=>start(true);
const labels={discovering:'Reading DICOM headers…',converting:'Converting and checking EPI series…',prepared:'Preparing secure multipart upload…',uploading:'Uploading approved bundles…',upload_failed:'Upload paused',complete:'Upload complete',dry_run_complete:'Local dry run complete',complete_no_eligible_series:'No eligible EPI series found',failed:'Local validation stopped'};
function resetChooser(){folder=null;$('folderPath').textContent='No folder selected';$('approval').checked=false;$('approval').disabled=false;$('pickBtn').disabled=false;buttons()}
async function poll(){try{const s=await api('/api/status/'+runId);$('stage').textContent=labels[s.status]||s.status;$('dicoms').textContent=s.summary.dicom_files;$('accepted').textContent=s.summary.accepted;$('held').textContent=s.summary.held;$('excluded').textContent=s.summary.excluded;const terminal=['complete','dry_run_complete','complete_no_eligible_series','failed','upload_failed'].includes(s.status);if(terminal){$('progress').classList.add('hidden');if(s.status==='upload_failed'){$('resumeBtn').classList.remove('hidden');$('result').textContent='Your prepared files are safe locally. Resume transfers only missing parts.'}else if(s.status==='failed'){$('result').textContent='Local preparation stopped. Nothing was uploaded.';resetChooser()}else{const r=await api('/api/report/'+runId);const commits=r.archive_commit_count?` · ${r.archive_commit_count} new archive commits`:'';const existing=r.existing_bundle_count?` · ${r.existing_bundle_count} already archived`:'';$('result').textContent=`${r.source_summary.accepted} EPI series prepared${commits}${existing} · ${formatBytes(r.bytes_prepared)} · ${r.status.replaceAll('_',' ')}`;$('resumeBtn').classList.add('hidden');resetChooser();await discoverResumable()}}else{pollTimer=setTimeout(poll,1500)}}catch(e){err(e)}}
$('resumeBtn').onclick=async()=>{try{$('resumeBtn').classList.add('hidden');$('progress').classList.remove('hidden');$('result').textContent=resumeMode==='reprepare'?'Revalidating the original source locally with current privacy rules…':'Resuming from the local checkpoint…';const r=await api('/api/'+resumeMode+'/'+runId,{method:'POST'});if(resumeMode==='reprepare'){runId=r.run_id;resumeMode='resume'}poll()}catch(e){$('progress').classList.add('hidden');$('resumeBtn').classList.remove('hidden');err(e)}};function formatBytes(n){if(!n)return '0 B';const u=['B','KB','MB','GB','TB'];const i=Math.min(Math.floor(Math.log(n)/Math.log(1024)),4);return (n/1024**i).toFixed(i?1:0)+' '+u[i]}load();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_uses_native_picker_endpoint_and_has_no_file_input() {
        assert!(INDEX_HTML.contains("/api/pick-folder"));
        assert!(INDEX_HTML.contains("/api/resumable"));
        assert!(INDEX_HTML.contains("Revalidate with current privacy rules"));
        assert!(INDEX_HTML.contains("runId=r.run_id;resumeMode='resume'"));
        assert!(INDEX_HTML.contains("location.hash.slice(1)"));
        assert!(INDEX_HTML.contains("sessionStorage.setItem('neuro-sync-token'"));
        assert!(INDEX_HTML.contains("sessionStorage.getItem('neuro-sync-token'"));
        assert!(INDEX_HTML.contains("history.replaceState"));
        assert!(!INDEX_HTML.contains("__TOKEN__"));
        assert!(!INDEX_HTML.contains("neuro-sync-token\" content"));
        assert!(INDEX_HTML.contains("Interrupted upload ready to resume"));
        assert!(!INDEX_HTML.contains("type=\"file\""));
        assert!(INDEX_HTML.contains("approval_confirmed"));
        assert!(INDEX_HTML.contains("/api/register"));
        assert!(INDEX_HTML.contains("Tell us which lab is contributing"));
        assert!(!INDEX_HTML.contains("One-time invite"));
    }

    #[tokio::test]
    async fn resumable_endpoint_returns_only_public_checkpoint_state() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let runtime = Runtime::initialize(Some(directory.path())).expect("runtime");
        runtime
            .state
            .create_run(
                "11111111-1111-4111-8111-111111111111",
                std::path::Path::new("/private/patient-folder"),
                false,
            )
            .expect("run state");
        runtime
            .state
            .update_run(
                "11111111-1111-4111-8111-111111111111",
                "prepared",
                &crate::model::SourceSummary {
                    dicom_files: 20,
                    accepted: 1,
                    ..Default::default()
                },
                None,
            )
            .expect("prepared state");

        let state = UiState {
            runtime,
            token: "test-token".into(),
            html: Arc::new(String::new()),
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-neuro-sync-token", "test-token".parse().expect("header"));
        let Json(value) = resumable(State(state), headers)
            .await
            .ok()
            .expect("resumable response");
        assert_eq!(value["pending_count"], 1);
        assert_eq!(value["active"], false);
        assert_eq!(value["requires_reprepare"], true);
        assert_eq!(
            value["next_run"]["id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(value["next_run"]["summary"]["accepted"], 1);
        let serialized = serde_json::to_string(&value).expect("response JSON");
        assert!(!serialized.contains("patient-folder"));
        assert!(!serialized.contains("source_path"));
        assert!(!serialized.contains("manifest_path"));
        assert!(!serialized.contains("report_path"));
    }

    #[test]
    fn listener_type_is_loopback() {
        let address: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert!(address.ip().is_loopback());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_picker_is_fixed_native_script_and_parses_cancel() {
        assert!(MACOS_FOLDER_PICKER_SCRIPT.contains("choose folder"));
        assert!(!MACOS_FOLDER_PICKER_SCRIPT.contains("do shell script"));
        assert_eq!(
            parse_macos_folder_picker_output(b"__NEURO_SYNC_FOLDER_CANCELLED__\n").unwrap(),
            None
        );
        assert_eq!(
            parse_macos_folder_picker_output(b"/tmp/DICOM Session/\n").unwrap(),
            Some(PathBuf::from("/tmp/DICOM Session/"))
        );
    }
}
