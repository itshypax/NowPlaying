use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CONFIG_HTML: &str = include_str!("../public/config.html");
const OVERLAY_HTML: &str = include_str!("../public/overlay.html");
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

// Field names and the millisecond timestamp match the data.json the Bun server wrote,
// so an existing install keeps its login after switching binaries.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_config: Option<Value>,
    // Fields from older versions survive a save instead of being dropped.
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

struct App {
    // An async lock, because a token refresh holds it across the request to Spotify.
    // Spotify rotates the refresh token on PKCE, so two parallel refreshes could
    // leave a stale one in data.json.
    store: tokio::sync::Mutex<Store>,
    // state -> code_verifier, only kept for the login round-trip
    pending: Mutex<HashMap<String, String>>,
    http: reqwest::Client,
    data_file: PathBuf,
    redirect_uri: String,
}

type Shared = State<Arc<App>>;

impl App {
    fn save(&self, store: &Store) {
        let text = serde_json::to_string_pretty(store).expect("store serializes");
        if let Err(e) = std::fs::write(&self.data_file, text) {
            eprintln!("Could not write {}: {e}", self.data_file.display());
        }
    }

    async fn access_token(&self) -> Option<String> {
        let mut store = self.store.lock().await;
        let (Some(client_id), Some(refresh)) = (store.client_id.clone(), store.refresh_token.clone()) else {
            return None;
        };
        if let (Some(token), Some(expires)) = (&store.access_token, store.expires_at) {
            if now_ms() + 30_000 < expires {
                return Some(token.clone());
            }
        }
        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &client_id)])
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            let status = res.status();
            eprintln!("Token refresh failed: {status} {}", res.text().await.unwrap_or_default());
            return None;
        }
        let data: TokenResponse = res.json().await.ok()?;
        store.access_token = Some(data.access_token.clone());
        store.expires_at = Some(now_ms() + data.expires_in * 1000);
        if data.refresh_token.is_some() {
            store.refresh_token = data.refresh_token;
        }
        self.save(&store);
        Some(data.access_token)
    }

    async fn spotify_get(&self, token: &str, url: &str) -> Result<reqwest::Response, Response> {
        self.http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Spotify not reachable: {e}")).into_response())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn random_string(len: usize) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn cover_host_allowed(url: &str) -> Option<bool> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(parsed.scheme() == "https" && (host.ends_with(".scdn.co") || host.ends_with(".spotifycdn.com")))
}

fn artists(item: &Value) -> String {
    item["artists"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

// Spotify lists album images large to small; the overlay wants a mid-sized one.
fn image(item: &Value, preferred: &[usize]) -> Value {
    let images = item["album"]["images"].as_array();
    preferred
        .iter()
        .find_map(|i| images.and_then(|imgs| imgs.get(*i)).and_then(|img| img["url"].as_str()))
        .map_or(Value::Null, |u| Value::String(u.to_string()))
}

async fn status(State(app): Shared) -> Json<Value> {
    let store = app.store.lock().await;
    Json(json!({
        "configured": store.client_id.is_some(),
        "authed": store.refresh_token.is_some(),
        "redirect_uri": app.redirect_uri,
    }))
}

async fn set_client_id(State(app): Shared, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(id) = body["client_id"].as_str().map(str::trim).filter(|s| s.chars().count() >= 10) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_client_id" }))).into_response();
    };
    let mut store = app.store.lock().await;
    store.client_id = Some(id.to_string());
    app.save(&store);
    Json(json!({ "ok": true })).into_response()
}

async fn get_config(State(app): Shared) -> Json<Value> {
    Json(app.store.lock().await.ui_config.clone().unwrap_or_else(|| json!({})))
}

// The config page persists its control values here so they survive restarts.
async fn set_config(State(app): Shared, body: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "bad_config" }))).into_response(),
    };
    if body.to_string().len() > 4000 {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "too_large" }))).into_response();
    }
    let mut store = app.store.lock().await;
    store.ui_config = Some(body);
    app.save(&store);
    Json(json!({ "ok": true })).into_response()
}

async fn login(State(app): Shared) -> Response {
    let Some(client_id) = app.store.lock().await.client_id.clone() else {
        return (StatusCode::BAD_REQUEST, "Save a client ID first (config page).").into_response();
    };
    let verifier = random_string(64);
    let state = random_string(16);
    let challenge = pkce_challenge(&verifier);
    app.pending.lock().unwrap().insert(state.clone(), verifier);
    let url = reqwest::Url::parse_with_params(
        "https://accounts.spotify.com/authorize",
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("scope", "user-read-currently-playing user-read-playback-state"),
            ("redirect_uri", &app.redirect_uri),
            ("state", &state),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ],
    )
    .expect("static authorize url");
    Redirect::to(url.as_str()).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(State(app): Shared, Query(q): Query<CallbackQuery>) -> Response {
    let verifier = app.pending.lock().unwrap().remove(q.state.as_deref().unwrap_or(""));
    let client_id = app.store.lock().await.client_id.clone();
    let (Some(code), Some(verifier), Some(client_id)) = (q.code, verifier, client_id) else {
        return (StatusCode::BAD_REQUEST, "Callback without a valid code/state. Try again via /login.").into_response();
    };
    let res = app
        .http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &app.redirect_uri),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
        ])
        .send()
        .await;
    let res = match res {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Token exchange failed: {text}")).into_response();
        }
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("Token exchange failed: {e}")).into_response(),
    };
    let Ok(data) = res.json::<TokenResponse>().await else {
        return (StatusCode::BAD_GATEWAY, "Token exchange returned an unexpected answer.").into_response();
    };
    let mut store = app.store.lock().await;
    store.access_token = Some(data.access_token);
    store.refresh_token = data.refresh_token;
    store.expires_at = Some(now_ms() + data.expires_in * 1000);
    app.save(&store);
    Redirect::to("/?connected=1").into_response()
}

async fn now_playing(State(app): Shared) -> Response {
    let Some(token) = app.access_token().await else {
        return Json(json!({ "playing": false, "authed": false })).into_response();
    };
    let res = match app.spotify_get(&token, "https://api.spotify.com/v1/me/player/currently-playing").await {
        Ok(r) => r,
        Err(e) => return e,
    };
    // 204 = nothing playing right now
    if res.status() == StatusCode::NO_CONTENT {
        return Json(json!({ "playing": false, "authed": true })).into_response();
    }
    if !res.status().is_success() {
        return Json(json!({ "playing": false, "authed": true, "error": res.status().as_u16() })).into_response();
    }
    let data: Value = res.json().await.unwrap_or(Value::Null);
    let item = &data["item"];
    if !item.is_object() || data["currently_playing_type"] != "track" {
        return Json(json!({ "playing": false, "authed": true })).into_response();
    }
    Json(json!({
        "playing": data["is_playing"] == true,
        "authed": true,
        "id": item["id"],
        "title": item["name"],
        "artists": artists(item),
        "cover": image(item, &[1, 0]),
        "progress_ms": data["progress_ms"].as_u64().unwrap_or(0),
        "duration_ms": item["duration_ms"].as_u64().unwrap_or(0),
    }))
    .into_response()
}

// Next track from the queue, feeds the "Up Next" banner
async fn next(State(app): Shared) -> Response {
    let Some(token) = app.access_token().await else {
        return Json(json!({})).into_response();
    };
    let res = match app.spotify_get(&token, "https://api.spotify.com/v1/me/player/queue").await {
        Ok(r) => r,
        Err(e) => return e,
    };
    // 403 here almost always means: no Premium (the queue endpoint is Premium-only)
    if !res.status().is_success() {
        return Json(json!({ "error": res.status().as_u16() })).into_response();
    }
    let data: Value = res.json().await.unwrap_or(Value::Null);
    let next = data["queue"].as_array().and_then(|q| q.iter().find(|i| i["type"] == "track"));
    let Some(next) = next else {
        return Json(json!({})).into_response();
    };
    Json(json!({
        "title": next["name"],
        "artists": artists(next),
        "cover": image(next, &[2, 1, 0]),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CoverQuery {
    url: Option<String>,
}

// Covers are proxied so the canvas color extraction doesn't run into CORS.
async fn cover(State(app): Shared, Query(q): Query<CoverQuery>) -> Response {
    let url = q.url.unwrap_or_default();
    match cover_host_allowed(&url) {
        None => return (StatusCode::BAD_REQUEST, "bad url").into_response(),
        Some(false) => return (StatusCode::FORBIDDEN, "host not allowed").into_response(),
        Some(true) => {}
    }
    let res = match app.http.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return (StatusCode::BAD_GATEWAY, "fetch failed").into_response(),
    };
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    match res.bytes().await {
        Ok(body) => (
            [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=86400".to_string())],
            body,
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "fetch failed").into_response(),
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8976);
    let root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let data_file = root.join("data.json");
    let store = std::fs::read_to_string(&data_file)
        .ok()
        .and_then(|s| match serde_json::from_str(&s) {
            Ok(store) => Some(store),
            Err(e) => {
                eprintln!("Ignoring unreadable {}: {e}", data_file.display());
                None
            }
        })
        .unwrap_or_default();

    let app = Arc::new(App {
        store: tokio::sync::Mutex::new(store),
        pending: Mutex::new(HashMap::new()),
        http: reqwest::Client::new(),
        data_file,
        redirect_uri: format!("http://127.0.0.1:{port}/callback"),
    });

    let router = Router::new()
        .route("/", get(|| async { Html(CONFIG_HTML) }))
        .route("/overlay", get(|| async { Html(OVERLAY_HTML) }))
        .route("/api/status", get(status))
        .route("/api/client-id", axum::routing::post(set_client_id))
        .route("/api/config", get(get_config).post(set_config))
        .route("/login", get(login))
        .route("/callback", get(callback))
        .route("/api/now-playing", get(now_playing))
        .route("/api/next", get(next))
        .route("/api/cover", get(cover))
        .with_state(app.clone());

    // All interfaces, like the Bun server, so OBS on a second PC can still reach it.
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Port {port} is not available: {e}");
            std::process::exit(1);
        }
    };
    println!("Running at http://127.0.0.1:{port}");
    println!("Redirect URI for your Spotify app: {}", app.redirect_uri);
    axum::serve(listener, router).await.expect("server");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn cover_proxy_only_fetches_spotify_cdn() {
        assert_eq!(cover_host_allowed("https://i.scdn.co/image/abc"), Some(true));
        assert_eq!(cover_host_allowed("https://image-cdn-ak.spotifycdn.com/x"), Some(true));
        assert_eq!(cover_host_allowed("https://evil.com/?x=.scdn.co"), Some(false));
        assert_eq!(cover_host_allowed("https://scdn.co.evil.com/x"), Some(false));
        assert_eq!(cover_host_allowed("http://i.scdn.co/image/abc"), Some(false));
        assert_eq!(cover_host_allowed("not a url"), None);
    }

    #[test]
    fn old_bun_data_file_still_loads() {
        let s: Store = serde_json::from_str(
            r#"{"client_id":"abcdefghij","refresh_token":"r","access_token":"a","expires_at":1753000000000,"ui_config":{"w":420},"source":"system"}"#,
        )
        .unwrap();
        assert_eq!(s.expires_at, Some(1753000000000));
        assert_eq!(serde_json::to_value(&s).unwrap()["source"], "system");
        assert_eq!(s.ui_config.unwrap()["w"], 420);
    }
}
