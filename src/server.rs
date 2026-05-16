use crate::store::Store;
use crate::{CalendarFile, Event};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Client;
use scraper::{Html as ParsedHtml, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
    pub addr: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    data_dir: PathBuf,
    output_dir: PathBuf,
    client: Client,
    ai_defaults: AiDefaults,
}

#[derive(Debug, Clone, Default, Serialize)]
struct AiDefaults {
    base_url: String,
    model: String,
    api_key: String,
}

pub async fn serve(options: ServerOptions) -> Result<(), crate::BoxError> {
    let store = Store::new(options.db_path);
    store.init()?;

    let state = AppState {
        store,
        data_dir: options.data_dir,
        output_dir: options.output_dir,
        client: Client::new(),
        ai_defaults: load_ai_defaults(FsPath::new(".env")),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(get_config))
        .route("/api/calendars", get(list_calendars))
        .route("/api/calendars/{id}", get(get_calendar).put(save_calendar))
        .route("/api/import-json", post(import_json))
        .route("/api/export-json", post(export_json))
        .route("/api/generate", post(generate))
        .route("/api/import/preview", post(import_preview))
        .route("/api/import/apply", post(import_apply))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(options.addr).await?;
    println!("Serving editor at http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_config(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ai": state.ai_defaults
    }))
}

async fn list_calendars(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let calendars = state.store.list_calendars()?;
    Ok(Json(json!({ "calendars": calendars })))
}

async fn get_calendar(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<CalendarFile>> {
    Ok(Json(state.store.load_calendar(&id)?))
}

async fn save_calendar(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(calendar): Json<CalendarFile>,
) -> ApiResult<Json<Value>> {
    if id != calendar.id {
        return Err(ApiError::bad_request("path id does not match calendar id"));
    }
    state.store.save_calendar(&calendar)?;
    Ok(Json(json!({ "saved": calendar.id })))
}

async fn import_json(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let imported = state.store.import_json_dir(&state.data_dir)?;
    Ok(Json(json!({ "imported": imported })))
}

async fn export_json(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let written = state.store.export_json_dir(&state.data_dir)?;
    Ok(Json(json!({ "written": display_paths(written) })))
}

async fn generate(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let written = state
        .store
        .export_json_and_generate(&state.data_dir, &state.output_dir)?;
    Ok(Json(json!({ "written": display_paths(written) })))
}

#[derive(Debug, Deserialize)]
struct PreviewRequest {
    calendar_id: String,
    url: Option<String>,
    text: Option<String>,
    ai: AiConfig,
}

#[derive(Debug, Deserialize)]
struct AiConfig {
    base_url: String,
    model: String,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct PreviewResponse {
    import_id: i64,
    source_url: Option<String>,
    raw_text: String,
    events: Vec<Event>,
}

async fn import_preview(
    State(state): State<AppState>,
    Json(request): Json<PreviewRequest>,
) -> ApiResult<Json<PreviewResponse>> {
    if request.calendar_id.trim().is_empty() {
        return Err(ApiError::bad_request("calendar_id is required"));
    }

    let raw_text = match request.text.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => {
            fetch_url_text(
                &state.client,
                request
                    .url
                    .as_deref()
                    .ok_or_else(|| ApiError::bad_request("url or text is required"))?,
            )
            .await?
        }
    };

    let extracted = extract_events_with_ai(&state.client, &request.ai, &raw_text).await?;
    let mut events = extracted.events;
    for event in &mut events {
        if event.url.is_none() {
            event.url = request.url.clone();
        }
    }

    let extracted_json = serde_json::to_string_pretty(&events)?;
    let import_id = state.store.record_import(
        &request.calendar_id,
        request.url.as_deref(),
        &raw_text,
        &extracted_json,
    )?;

    Ok(Json(PreviewResponse {
        import_id,
        source_url: request.url,
        raw_text,
        events,
    }))
}

#[derive(Debug, Deserialize)]
struct ApplyRequest {
    calendar_id: String,
    import_id: Option<i64>,
    events: Vec<Event>,
}

async fn import_apply(
    State(state): State<AppState>,
    Json(request): Json<ApplyRequest>,
) -> ApiResult<Json<Value>> {
    let applied = state
        .store
        .upsert_events(&request.calendar_id, &request.events)?;
    if let Some(import_id) = request.import_id {
        state.store.mark_import_applied(import_id)?;
    }
    Ok(Json(json!({ "applied": applied })))
}

#[derive(Debug, Deserialize)]
struct ExtractedEvents {
    events: Vec<Event>,
}

async fn fetch_url_text(client: &Client, url: &str) -> ApiResult<String> {
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|err| ApiError::bad_gateway(format!("failed to fetch url: {err}")))?
        .error_for_status()
        .map_err(|err| ApiError::bad_gateway(format!("url returned an error: {err}")))?
        .text()
        .await
        .map_err(|err| ApiError::bad_gateway(format!("failed to read response text: {err}")))?;
    Ok(extract_visible_text(&body))
}

fn extract_visible_text(body: &str) -> String {
    let document = ParsedHtml::parse_document(body);
    let selector = Selector::parse("body").expect("static selector is valid");
    let mut text = String::new();
    for element in document.select(&selector) {
        for part in element.text() {
            let part = part.trim();
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
    }

    if text.is_empty() {
        body.to_string()
    } else {
        text
    }
}

async fn extract_events_with_ai(
    client: &Client,
    config: &AiConfig,
    raw_text: &str,
) -> ApiResult<ExtractedEvents> {
    if config.base_url.trim().is_empty()
        || config.model.trim().is_empty()
        || config.api_key.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "ai.base_url, ai.model and ai.api_key are required",
        ));
    }

    let endpoint = chat_completions_endpoint(&config.base_url);
    let input = truncate_chars(raw_text, 20000);
    let body = json!({
        "model": config.model,
        "temperature": 0,
        "response_format": { "type": "json_object" },
        "messages": [
            {
                "role": "system",
                "content": "Extract exam calendar events from official Chinese exam notices. Return strict JSON only, with an events array. Each event must match this schema: id, title, start, end, all_day, description, location, url, status, alarm_minutes. Use YYYY-MM-DD for all-day dates and YYYY-MM-DDTHH:MM:SS for timed events. For all-day events, end must be the exclusive end date. If unsure, use all_day true and put uncertainty in description. Do not invent dates."
            },
            {
                "role": "user",
                "content": input
            }
        ]
    });

    let response: Value = client
        .post(endpoint)
        .bearer_auth(config.api_key.trim())
        .json(&body)
        .send()
        .await
        .map_err(|err| ApiError::bad_gateway(format!("ai request failed: {err}")))?
        .error_for_status()
        .map_err(|err| ApiError::bad_gateway(format!("ai returned an error: {err}")))?
        .json()
        .await
        .map_err(|err| ApiError::bad_gateway(format!("ai response was not JSON: {err}")))?;

    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_gateway("ai response did not contain message content"))?;
    parse_extracted_events(content)
}

fn parse_extracted_events(content: &str) -> ApiResult<ExtractedEvents> {
    let content = content.trim();
    if let Ok(events) = serde_json::from_str::<ExtractedEvents>(content) {
        return Ok(events);
    }
    if let Ok(events) = serde_json::from_str::<Vec<Event>>(content) {
        return Ok(ExtractedEvents { events });
    }

    let Some(start) = content.find('{') else {
        return Err(ApiError::bad_gateway("ai output did not contain JSON"));
    };
    let Some(end) = content.rfind('}') else {
        return Err(ApiError::bad_gateway("ai output did not contain JSON"));
    };
    serde_json::from_str::<ExtractedEvents>(&content[start..=end])
        .map_err(|err| ApiError::bad_gateway(format!("failed to parse ai events: {err}")))
}

fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn load_ai_defaults(env_path: &FsPath) -> AiDefaults {
    let file_values = read_env_file(env_path);
    AiDefaults {
        base_url: env_value(&file_values, &["AI_BASE_URL", "OPENAI_BASE_URL"]),
        model: env_value(&file_values, &["AI_MODEL", "OPENAI_MODEL"]),
        api_key: env_value(&file_values, &["AI_API_KEY", "OPENAI_API_KEY"]),
    }
}

fn read_env_file(path: &FsPath) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        values.insert(key.to_string(), strip_env_quotes(value.trim()).to_string());
    }
    values
}

fn strip_env_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn env_value(file_values: &HashMap<String, String>, keys: &[&str]) -> String {
    for key in keys {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                return value;
            }
        }
        if let Some(value) = file_values.get(*key) {
            if !value.trim().is_empty() {
                return value.clone();
            }
        }
    }
    String::new()
}

fn display_paths(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.display().to_string())
        .collect()
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message
            })),
        )
            .into_response()
    }
}

impl From<crate::BoxError> for ApiError {
    fn from(error: crate::BoxError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Exam Calendar Studio</title>
  <style>
    :root {
      color-scheme: light;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background:
        radial-gradient(circle at 50% -12%, rgba(148, 163, 184, .24), transparent 34%),
        radial-gradient(circle at 16% 18%, rgba(203, 213, 225, .26), transparent 26%),
        radial-gradient(circle at 84% 22%, rgba(186, 199, 214, .22), transparent 28%),
        linear-gradient(135deg, #f3f6fb 0%, #fbfcfe 48%, #eef2f7 100%);
      color: #111827;
      font-synthesis: none;
    }

    * { box-sizing: border-box; }
    body { margin: 0; min-height: 100vh; }
    body::before {
      content: "";
      position: fixed;
      inset: 0;
      pointer-events: none;
      background-image: linear-gradient(rgba(255,255,255,.38) 1px, transparent 1px), linear-gradient(90deg, rgba(255,255,255,.34) 1px, transparent 1px);
      background-size: 42px 42px;
      mask-image: linear-gradient(to bottom, rgba(0,0,0,.8), transparent 72%);
    }

    .shell {
      width: min(1440px, calc(100vw - 32px));
      margin: 0 auto;
      padding: 18px 0 28px;
    }

    .topbar {
      position: sticky;
      top: 12px;
      z-index: 10;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      padding: 14px 16px;
      border: 1px solid rgba(255, 255, 255, .66);
      border-radius: 24px;
      background: rgba(255, 255, 255, .58);
      box-shadow: 0 18px 55px rgba(33, 51, 86, .16), inset 0 1px 0 rgba(255,255,255,.88);
      backdrop-filter: blur(24px) saturate(165%);
      -webkit-backdrop-filter: blur(24px) saturate(165%);
    }

    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
      min-width: 230px;
    }

    .mark {
      display: grid;
      place-items: center;
      width: 42px;
      height: 42px;
      border-radius: 16px;
      color: #fff;
      font-weight: 780;
      background: linear-gradient(135deg, #2563eb, #14b8a6);
      box-shadow: 0 14px 28px rgba(37, 99, 235, .28);
    }

    h1 { margin: 0; font-size: 18px; line-height: 1.15; letter-spacing: 0; }
    .subtle { color: #64748b; font-size: 13px; line-height: 1.35; }

    .actions {
      display: flex;
      align-items: center;
      justify-content: flex-end;
      gap: 10px;
      flex-wrap: wrap;
    }

    .layout {
      display: grid;
      grid-template-columns: minmax(300px, 360px) minmax(0, 1fr);
      gap: 18px;
      margin-top: 18px;
      align-items: start;
    }

    .panel {
      position: relative;
      overflow: hidden;
      border: 1px solid rgba(255, 255, 255, .62);
      border-radius: 26px;
      background: rgba(255, 255, 255, .48);
      box-shadow: 0 20px 70px rgba(28, 45, 77, .14), inset 0 1px 0 rgba(255,255,255,.92);
      backdrop-filter: blur(28px) saturate(180%);
      -webkit-backdrop-filter: blur(28px) saturate(180%);
    }

    .panel::after {
      content: "";
      position: absolute;
      inset: 0;
      pointer-events: none;
      background: linear-gradient(135deg, rgba(255,255,255,.58), transparent 38%);
    }

    .panel-header, .panel-body { position: relative; z-index: 1; }
    .panel-header {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 12px;
      padding: 18px 18px 0;
    }

    h2 { margin: 0; font-size: 16px; line-height: 1.25; letter-spacing: 0; }
    .panel-body { padding: 16px 18px 18px; }
    .stack { display: grid; gap: 14px; }
    .grid-2 { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }

    label {
      display: block;
      margin: 0 0 6px;
      color: #334155;
      font-size: 12px;
      font-weight: 720;
    }

    input, select, textarea {
      width: 100%;
      min-width: 0;
      border: 1px solid rgba(148, 163, 184, .42);
      border-radius: 16px;
      background: rgba(255, 255, 255, .68);
      color: #111827;
      box-shadow: inset 0 1px 0 rgba(255,255,255,.72);
      font: inherit;
      outline: none;
      transition: border-color .16s ease, box-shadow .16s ease, background .16s ease;
    }

    input, select { height: 42px; padding: 0 13px; }
    textarea {
      min-height: 184px;
      resize: vertical;
      padding: 12px 13px;
      font-family: ui-monospace, SFMono-Regular, Consolas, "Liberation Mono", monospace;
      font-size: 12px;
      line-height: 1.55;
    }

    #calendarJson { min-height: 440px; }
    #eventsJson { min-height: 260px; }
    #sourceText { min-height: 122px; }

    input:focus, select:focus, textarea:focus {
      border-color: rgba(37, 99, 235, .72);
      background: rgba(255,255,255,.82);
      box-shadow: 0 0 0 4px rgba(37, 99, 235, .12), inset 0 1px 0 rgba(255,255,255,.8);
    }

    .button {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 8px;
      min-height: 40px;
      border: 1px solid transparent;
      border-radius: 999px;
      padding: 0 15px;
      font: inherit;
      font-weight: 720;
      cursor: pointer;
      white-space: nowrap;
      transition: transform .14s ease, box-shadow .14s ease, background .14s ease;
    }

    .button:active { transform: scale(.98); }
    .button:disabled {
      cursor: not-allowed;
      opacity: .62;
      transform: none;
      box-shadow: none;
    }
    .button.primary {
      color: #fff;
      background: linear-gradient(135deg, #2563eb, #0f766e);
      box-shadow: 0 12px 26px rgba(37, 99, 235, .24);
    }
    .button.secondary {
      color: #1e3a8a;
      border-color: rgba(255,255,255,.72);
      background: rgba(255,255,255,.64);
      box-shadow: 0 10px 22px rgba(30, 58, 138, .08), inset 0 1px 0 rgba(255,255,255,.86);
      backdrop-filter: blur(18px);
      -webkit-backdrop-filter: blur(18px);
    }

    .button.ghost {
      color: #475569;
      background: transparent;
      border-color: rgba(148, 163, 184, .24);
    }

    .segmented {
      display: inline-flex;
      gap: 4px;
      padding: 4px;
      border: 1px solid rgba(255,255,255,.62);
      border-radius: 999px;
      background: rgba(255,255,255,.44);
    }

    .status {
      display: flex;
      align-items: flex-start;
      gap: 10px;
      position: fixed;
      left: 20px;
      bottom: 20px;
      z-index: 60;
      width: min(420px, calc(100vw - 32px));
      min-height: 46px;
      white-space: pre-wrap;
      border: 1px solid rgba(255,255,255,.62);
      border-radius: 18px;
      padding: 12px 14px;
      color: #334155;
      background: rgba(255,255,255,.52);
      font-family: ui-monospace, SFMono-Regular, Consolas, "Liberation Mono", monospace;
      font-size: 12px;
      line-height: 1.5;
    }

    .status.loading::before {
      content: "";
      flex: 0 0 auto;
      width: 14px;
      height: 14px;
      margin-top: 1px;
      border-radius: 999px;
      border: 2px solid rgba(37, 99, 235, .22);
      border-top-color: #2563eb;
      animation: spin .8s linear infinite;
    }

    @keyframes spin {
      to { transform: rotate(360deg); }
    }

    .hint-row {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      color: #64748b;
      font-size: 12px;
    }

    .metric-grid {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 10px;
    }

    .metric {
      border: 1px solid rgba(255,255,255,.58);
      border-radius: 18px;
      padding: 12px;
      background: rgba(255,255,255,.46);
    }

    .metric strong {
      display: block;
      font-size: 20px;
      line-height: 1.1;
    }

    .metric span {
      display: block;
      margin-top: 4px;
      color: #64748b;
      font-size: 12px;
    }

    .toolbar {
      display: flex;
      gap: 8px;
      flex-wrap: wrap;
    }

    .toolbar .button { flex: 1; }

    .hidden, .draft-panel.hidden, .modal.hidden { display: none !important; }

    .modal {
      position: fixed;
      inset: 0;
      z-index: 40;
      display: grid;
      place-items: center;
      padding: 18px;
      background: rgba(15, 23, 42, .22);
      backdrop-filter: blur(10px);
      -webkit-backdrop-filter: blur(10px);
    }

    .modal-card {
      width: min(760px, 100%);
      max-height: min(760px, calc(100vh - 36px));
      overflow: auto;
      border: 1px solid rgba(255,255,255,.68);
      border-radius: 26px;
      background: rgba(255,255,255,.74);
      box-shadow: 0 28px 90px rgba(15, 23, 42, .24), inset 0 1px 0 rgba(255,255,255,.9);
      backdrop-filter: blur(28px) saturate(180%);
      -webkit-backdrop-filter: blur(28px) saturate(180%);
    }

    .modal-header {
      display: flex;
      justify-content: space-between;
      gap: 14px;
      padding: 18px 18px 0;
    }

    .modal-body {
      display: grid;
      gap: 14px;
      padding: 16px 18px 18px;
    }

    .toast-stack {
      position: fixed;
      top: 20px;
      right: 20px;
      z-index: 60;
      display: grid;
      gap: 10px;
      width: min(360px, calc(100vw - 32px));
    }

    .toast {
      border: 1px solid rgba(255,255,255,.68);
      border-radius: 18px;
      padding: 12px 14px;
      background: rgba(255,255,255,.74);
      box-shadow: 0 18px 48px rgba(15, 23, 42, .16);
      backdrop-filter: blur(24px) saturate(180%);
      -webkit-backdrop-filter: blur(24px) saturate(180%);
      color: #0f172a;
      font-size: 13px;
      line-height: 1.4;
    }

    .toast.error {
      border-color: rgba(239, 68, 68, .34);
      color: #7f1d1d;
      background: rgba(255, 241, 242, .82);
    }

    .source-mode {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 8px;
      padding: 4px;
      border: 1px solid rgba(255,255,255,.62);
      border-radius: 999px;
      background: rgba(255,255,255,.44);
    }

    .source-mode .button.active {
      color: #fff;
      background: linear-gradient(135deg, #2563eb, #0f766e);
      box-shadow: 0 10px 22px rgba(37, 99, 235, .18);
    }

    @media (max-width: 980px) {
      .shell { width: min(100vw - 20px, 760px); padding-top: 10px; }
      .topbar { position: static; align-items: stretch; flex-direction: column; border-radius: 22px; }
      .actions { justify-content: flex-start; }
      .layout { grid-template-columns: 1fr; }
      .grid-2 { grid-template-columns: 1fr; }
      #calendarJson { min-height: 320px; }
    }

    @media (max-width: 560px) {
      .actions, .segmented { width: 100%; }
      .button { flex: 1; padding: 0 10px; }
      .panel-header { flex-direction: column; }
    }
  </style>
</head>
<body>
  <div class="shell">
    <header class="topbar">
      <div class="brand">
        <div class="mark">EC</div>
        <div>
          <h1>Exam Calendar Studio</h1>
          <div class="subtle" id="activeCalendarTitle">本地维护工具</div>
        </div>
      </div>
      <div class="actions">
        <button id="openSettings" class="button secondary">设置</button>
        <button id="importJson" class="button secondary" title="从 data/calendars 导入">导入 JSON</button>
        <button id="generate" class="button primary" title="导出 JSON 并生成 ICS">生成 ICS</button>
      </div>
    </header>

    <main class="layout">
      <section class="panel">
        <div class="panel-header">
          <div>
            <h2>日历</h2>
            <div class="subtle" id="calendarMeta">未加载</div>
          </div>
          <button id="reloadCalendar" class="button ghost">刷新</button>
        </div>
        <div class="panel-body stack">
          <div>
            <label for="calendarSelect">当前日历</label>
            <select id="calendarSelect"></select>
          </div>
          <div class="metric-grid">
            <div class="metric">
              <strong id="eventCount">0</strong>
              <span>事件</span>
            </div>
            <div class="metric">
              <strong id="calendarYear">-</strong>
              <span>年份</span>
            </div>
          </div>
          <div class="toolbar">
            <button id="openCalendarJson" class="button secondary">编辑 JSON</button>
            <button id="exportJson" class="button secondary" title="导出到 data/calendars">导出 JSON</button>
          </div>
        </div>
      </section>

      <section class="panel">
        <div class="panel-header">
          <div>
            <h2>导入日程</h2>
            <div class="subtle">AI 生成草稿，人工确认</div>
          </div>
        </div>
        <div class="panel-body stack">
          <div class="source-mode">
            <button id="modeUrl" class="button active">URL</button>
            <button id="modeText" class="button secondary">正文</button>
          </div>
          <div>
            <label for="sourceUrl">官网 URL</label>
            <input id="sourceUrl" placeholder="https://www.example.gov.cn/notice.html">
          </div>
          <div id="sourceTextWrap" class="hidden">
            <label for="sourceText">公告正文</label>
            <textarea id="sourceText" placeholder="粘贴正文时优先使用正文"></textarea>
          </div>
          <div class="toolbar">
            <button id="previewImport" class="button primary">提取草稿</button>
          </div>
          <div id="draftPanel" class="draft-panel hidden stack">
            <div class="hint-row">
              <label for="eventsJson">事件草稿 JSON</label>
              <span id="draftCount">0 个事件</span>
            </div>
            <textarea id="eventsJson" spellcheck="false"></textarea>
            <button id="applyImport" class="button primary">确认写入 SQLite</button>
          </div>
        </div>
      </section>
    </main>

    <div id="toastStack" class="toast-stack"></div>
    <div id="status" class="status hidden"></div>

    <div id="settingsModal" class="modal hidden" role="dialog" aria-modal="true" aria-labelledby="settingsTitle">
      <div class="modal-card">
        <div class="modal-header">
          <div>
            <h2 id="settingsTitle">AI 设置</h2>
            <div class="subtle">默认读取项目根目录 .env</div>
          </div>
          <button id="closeSettings" class="button ghost">关闭</button>
        </div>
        <div class="modal-body">
          <div class="grid-2">
            <div>
              <label for="aiBaseUrl">Base URL</label>
              <input id="aiBaseUrl" placeholder="https://api.openai.com/v1">
            </div>
            <div>
              <label for="aiModel">Model</label>
              <input id="aiModel" placeholder="gpt-4.1-mini">
            </div>
          </div>
          <div>
            <label for="aiKey">API Key</label>
            <input id="aiKey" type="password" autocomplete="off">
          </div>
          <div class="toolbar">
            <button id="saveSettings" class="button primary">保存设置</button>
          </div>
        </div>
      </div>
    </div>

    <div id="calendarModal" class="modal hidden" role="dialog" aria-modal="true" aria-labelledby="calendarJsonTitle">
      <div class="modal-card">
        <div class="modal-header">
          <div>
            <h2 id="calendarJsonTitle">Calendar JSON</h2>
            <div class="subtle">保存前会校验结构和日期</div>
          </div>
          <button id="closeCalendarJson" class="button ghost">关闭</button>
        </div>
        <div class="modal-body">
          <textarea id="calendarJson" spellcheck="false"></textarea>
          <div class="toolbar">
            <button id="saveCalendar" class="button primary">保存到 SQLite</button>
          </div>
        </div>
      </div>
    </div>
  </div>

  <script>
    const $ = (id) => document.getElementById(id);
    let importId = null;
    let activeCalendar = null;
    let sourceMode = "url";

    function status(message, loading = false) {
      $("status").classList.toggle("loading", loading);
      if (!message) {
        $("status").classList.add("hidden");
        $("status").textContent = "";
        return;
      }
      $("status").classList.remove("hidden");
      $("status").textContent = typeof message === "string" ? message : JSON.stringify(message, null, 2);
    }

    function toast(message, type = "success") {
      const item = document.createElement("div");
      item.className = `toast ${type}`;
      item.textContent = typeof message === "string" ? message : JSON.stringify(message);
      $("toastStack").appendChild(item);
      setTimeout(() => item.remove(), 3600);
    }

    function fail(error) {
      toast(error.message || String(error), "error");
    }

    function setExtractBusy(busy) {
      $("previewImport").disabled = busy;
      $("applyImport").disabled = busy;
      $("previewImport").textContent = busy ? "提取中..." : "提取草稿";
    }

    function setApplyBusy(busy) {
      $("applyImport").disabled = busy;
      $("previewImport").disabled = busy;
      $("applyImport").textContent = busy ? "写入中..." : "确认写入";
    }

    function openModal(id) {
      $(id).classList.remove("hidden");
    }

    function closeModal(id) {
      $(id).classList.add("hidden");
    }

    function setSourceMode(mode) {
      sourceMode = mode;
      $("modeUrl").classList.toggle("active", mode === "url");
      $("modeText").classList.toggle("active", mode === "text");
      $("modeUrl").classList.toggle("secondary", mode !== "url");
      $("modeText").classList.toggle("secondary", mode !== "text");
      $("sourceUrl").parentElement.classList.toggle("hidden", mode !== "url");
      $("sourceTextWrap").classList.toggle("hidden", mode !== "text");
    }

    async function api(path, options = {}) {
      const response = await fetch(path, {
        headers: { "Content-Type": "application/json", ...(options.headers || {}) },
        ...options
      });
      const text = await response.text();
      const data = text ? JSON.parse(text) : null;
      if (!response.ok) throw new Error(data && data.error ? data.error : text);
      return data;
    }

    async function loadConfig() {
      const config = await api("/api/config");
      const local = JSON.parse(localStorage.getItem("exam-calendar-ai") || "{}");
      if (config.ai) {
        $("aiBaseUrl").value = local.base_url || config.ai.base_url || "";
        $("aiModel").value = local.model || config.ai.model || "";
        $("aiKey").value = local.api_key || config.ai.api_key || "";
      }
    }

    async function loadCalendars() {
      const data = await api("/api/calendars");
      $("calendarSelect").innerHTML = "";
      for (const calendar of data.calendars) {
        const option = document.createElement("option");
        option.value = calendar.id;
        option.textContent = `${calendar.id} · ${calendar.title}`;
        $("calendarSelect").appendChild(option);
      }
      if (data.calendars.length) await loadCalendar();
    }

    async function loadCalendar() {
      const id = $("calendarSelect").value;
      if (!id) return;
      const calendar = await api(`/api/calendars/${encodeURIComponent(id)}`);
      activeCalendar = calendar;
      $("calendarJson").value = JSON.stringify(calendar, null, 2);
      $("activeCalendarTitle").textContent = calendar.title;
      $("calendarMeta").textContent = `${calendar.region} · ${calendar.exam_type}`;
      $("eventCount").textContent = String(calendar.events.length);
      $("calendarYear").textContent = String(calendar.year);
    }

    $("importJson").onclick = async () => {
      try {
        const data = await api("/api/import-json", { method: "POST", body: "{}" });
        await loadCalendars();
        toast(`已导入 ${data.imported.length} 个日历`);
      } catch (error) { fail(error); }
    };

    $("exportJson").onclick = async () => {
      try {
        const data = await api("/api/export-json", { method: "POST", body: "{}" });
        toast(`已导出 ${data.written.length} 个 JSON`);
      } catch (error) { fail(error); }
    };

    $("generate").onclick = async () => {
      try {
        const data = await api("/api/generate", { method: "POST", body: "{}" });
        toast(`已生成 ${data.written.length} 个 ICS`);
      } catch (error) { fail(error); }
    };

    $("reloadCalendar").onclick = () => loadCalendar().then(() => toast("已刷新")).catch(fail);
    $("calendarSelect").onchange = () => loadCalendar().catch(fail);
    $("openSettings").onclick = () => openModal("settingsModal");
    $("closeSettings").onclick = () => closeModal("settingsModal");
    $("openCalendarJson").onclick = () => openModal("calendarModal");
    $("closeCalendarJson").onclick = () => closeModal("calendarModal");
    $("modeUrl").onclick = () => setSourceMode("url");
    $("modeText").onclick = () => setSourceMode("text");

    $("saveSettings").onclick = () => {
      localStorage.setItem("exam-calendar-ai", JSON.stringify({
        base_url: $("aiBaseUrl").value,
        model: $("aiModel").value,
        api_key: $("aiKey").value
      }));
      closeModal("settingsModal");
      toast("AI 设置已保存");
    };

    $("saveCalendar").onclick = async () => {
      try {
        const calendar = JSON.parse($("calendarJson").value);
        await api(`/api/calendars/${encodeURIComponent(calendar.id)}`, {
          method: "PUT",
          body: JSON.stringify(calendar)
        });
        await loadCalendars();
        closeModal("calendarModal");
        toast("日历已保存");
      } catch (error) { fail(error); }
    };

    $("previewImport").onclick = async () => {
      setExtractBusy(true);
      status("正在抓取网页并调用 AI 提取事件草稿...", true);
      try {
        const payload = {
          calendar_id: $("calendarSelect").value,
          url: sourceMode === "url" ? ($("sourceUrl").value || null) : null,
          text: sourceMode === "text" ? ($("sourceText").value || null) : null,
          ai: {
            base_url: $("aiBaseUrl").value,
            model: $("aiModel").value,
            api_key: $("aiKey").value
          }
        };
        const data = await api("/api/import/preview", {
          method: "POST",
          body: JSON.stringify(payload)
        });
        importId = data.import_id;
        $("eventsJson").value = JSON.stringify(data.events, null, 2);
        $("draftCount").textContent = `${data.events.length} 个事件`;
        $("draftPanel").classList.remove("hidden");
        toast(`已生成 ${data.events.length} 个事件草稿`);
      } catch (error) {
        fail(error);
      } finally {
        status("");
        setExtractBusy(false);
      }
    };

    $("applyImport").onclick = async () => {
      setApplyBusy(true);
      status("正在校验并写入 SQLite...", true);
      try {
        const payload = {
          calendar_id: $("calendarSelect").value,
          import_id: importId,
          events: JSON.parse($("eventsJson").value)
        };
        const data = await api("/api/import/apply", {
          method: "POST",
          body: JSON.stringify(payload)
        });
        await loadCalendar();
        $("draftPanel").classList.add("hidden");
        $("eventsJson").value = "";
        toast(`已写入 ${data.applied} 个事件`);
      } catch (error) {
        fail(error);
      } finally {
        status("");
        setApplyBusy(false);
      }
    };

    setSourceMode("url");
    Promise.all([loadConfig(), loadCalendars()]).catch(fail);
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_base_url_or_full_chat_endpoint() {
        assert_eq!(
            chat_completions_endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://api.example.com/v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn parses_ai_events_object() {
        let parsed = parse_extracted_events(
            r#"{"events":[{"id":"main","title":"考试","start":"2026-06-07","end":"2026-06-10","all_day":true}]}"#,
        )
        .unwrap();

        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].id, "main");
        assert!(parsed.events[0].all_day);
    }

    #[test]
    fn reads_dotenv_values() {
        let path = std::env::temp_dir().join("exam-calendar-test.env");
        std::fs::write(
            &path,
            "AI_BASE_URL=\"https://api.example.com/v1\"\nAI_MODEL='model-a'\nAI_API_KEY=secret\n",
        )
        .unwrap();

        let values = read_env_file(&path);

        assert_eq!(
            values.get("AI_BASE_URL").unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(values.get("AI_MODEL").unwrap(), "model-a");
        assert_eq!(values.get("AI_API_KEY").unwrap(), "secret");
        let _ = std::fs::remove_file(path);
    }
}
