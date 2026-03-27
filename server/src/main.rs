use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use rcgen::generate_simple_self_signed;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Clone)]
#[command(name = "llm-viewer-server")]
#[command(about = "LLM Viewer Server with automatic HTTPS and SpacetimeDB persistence")]
struct Cli {
    /// Bind address
    #[arg(long, default_value = "0.0.0.0:3080", env = "BIND_ADDR")]
    bind: String,

    /// TLS certificate path (PEM). Omit with --key to auto-generate a self-signed cert.
    #[arg(long, env = "TLS_CERT_PATH")]
    cert: Option<String>,

    /// TLS private key path (PEM). Must be provided together with --cert.
    #[arg(long, env = "TLS_KEY_PATH")]
    key: Option<String>,

    /// API key for authentication
    #[arg(long, env = "API_KEY")]
    api_key: Option<String>,

    /// SpacetimeDB server URL
    #[arg(long, default_value = "http://127.0.0.1:3001", env = "STDB_SERVER")]
    stdb_server: String,

    /// SpacetimeDB database name
    #[arg(long, default_value = "harness", env = "STDB_DATABASE")]
    stdb_database: String,
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub content: String,
    pub content_type: ContentType,
    pub source: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Markdown,
    Html,
}

impl ContentType {
    fn as_str(&self) -> &'static str {
        match self {
            ContentType::Markdown => "markdown",
            ContentType::Html => "html",
        }
    }

    fn from_str_lossy(s: &str) -> Self {
        match s {
            "html" => ContentType::Html,
            _ => ContentType::Markdown,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PushRequest {
    pub title: String,
    pub content: String,
    pub content_type: ContentType,
    pub source: String,
    pub timestamp: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    items: Arc<RwLock<Vec<Item>>>,
    tx: broadcast::Sender<Item>,
    api_key: String,
    stdb: StdbClient,
}

// ---------------------------------------------------------------------------
// SpacetimeDB HTTP client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StdbClient {
    http: Client,
    server: String,
    database: String,
}

impl StdbClient {
    fn new(server: String, database: String) -> Self {
        Self {
            http: Client::new(),
            server,
            database,
        }
    }

    async fn sql_query(&self, query: &str) -> Result<Vec<Vec<serde_json::Value>>, String> {
        let url = format!("{}/v1/database/{}/sql", self.server, self.database);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(query.to_string())
            .send()
            .await
            .map_err(|e| format!("stdb sql request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("stdb sql failed ({status}): {body}"));
        }

        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("stdb sql json parse: {e}"))?;

        let rows = parsed
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|table| table.get("rows"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(rows
            .into_iter()
            .filter_map(|r| r.as_array().cloned())
            .collect())
    }

    async fn call_reducer(&self, reducer: &str, args: serde_json::Value) -> Result<(), String> {
        let url = format!(
            "{}/v1/database/{}/call/{}",
            self.server, self.database, reducer
        );
        let resp = self
            .http
            .post(&url)
            .json(&args)
            .send()
            .await
            .map_err(|e| format!("stdb call {reducer} failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("stdb call {reducer} failed ({status}): {body}"));
        }
        Ok(())
    }

    /// Insert a blob + ref into SpacetimeDB. Best-effort; logs errors but doesn't fail.
    async fn persist_item(&self, item: &Item) {
        let content_hash = sha256_hex(&item.content);
        let size_bytes = item.content.len() as u64;
        let content_type_str = item.content_type.as_str().to_string();
        let timestamp_str = item.timestamp.to_rfc3339();

        // Insert blob (reducer is idempotent on duplicate hash)
        if let Err(e) = self
            .call_reducer(
                "insert_blob",
                serde_json::json!([content_hash, item.content, content_type_str, size_bytes]),
            )
            .await
        {
            eprintln!("warn: stdb insert_blob: {e}");
        }

        // Insert ref
        if let Err(e) = self
            .call_reducer(
                "insert_ref",
                serde_json::json!([
                    item.id,
                    item.title,
                    content_hash,
                    item.source,
                    timestamp_str
                ]),
            )
            .await
        {
            eprintln!("warn: stdb insert_ref: {e}");
        }
    }

    /// Load all display refs + blobs from SpacetimeDB, ordered by timestamp.
    async fn load_history(&self) -> Vec<Item> {
        let query = "SELECT r.id, r.title, r.content_hash, r.source, r.timestamp, b.content, b.content_type \
                     FROM display_refs r JOIN display_blobs b ON r.content_hash = b.content_hash \
                     ORDER BY r.timestamp ASC";
        match self.sql_query(query).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| {
                    let id = row.first()?.as_str()?.to_string();
                    let title = row.get(1)?.as_str()?.to_string();
                    let source = row.get(3)?.as_str()?.to_string();
                    let timestamp_str = row.get(4)?.as_str()?;
                    let content = row.get(5)?.as_str()?.to_string();
                    let content_type_str = row.get(6)?.as_str()?;

                    let timestamp = DateTime::parse_from_rfc3339(timestamp_str)
                        .ok()?
                        .with_timezone(&Utc);

                    Some(Item {
                        id,
                        title,
                        content,
                        content_type: ContentType::from_str_lossy(content_type_str),
                        source,
                        timestamp,
                    })
                })
                .collect(),
            Err(e) => {
                eprintln!("warn: stdb load_history: {e}");
                Vec::new()
            }
        }
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// API key management
// ---------------------------------------------------------------------------

fn resolve_api_key(explicit: Option<String>) -> String {
    if let Some(key) = explicit {
        return key;
    }

    let key_path = std::path::Path::new(".api_key");
    if key_path.exists() {
        if let Ok(key) = std::fs::read_to_string(key_path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return key;
            }
        }
    }

    // Generate a new random 32-char hex key
    let mut rng = rand::thread_rng();
    let key: String = (0..32)
        .map(|_| format!("{:x}", rng.gen::<u8>() % 16))
        .collect();
    let _ = std::fs::write(key_path, &key);
    println!("Generated API key: {key}");
    key
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiKeyQuery {
    api_key: Option<String>,
}

async fn auth_middleware(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token_from_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    let token = token_from_header.or(query.api_key);

    match token {
        Some(t) if t == state.api_key => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn push_item(
    State(state): State<AppState>,
    Json(req): Json<PushRequest>,
) -> impl IntoResponse {
    let item = Item {
        id: Uuid::new_v4().to_string(),
        title: req.title,
        content: req.content,
        content_type: req.content_type,
        source: req.source,
        timestamp: req.timestamp.unwrap_or_else(Utc::now),
    };

    // Persist to SpacetimeDB (best-effort, async)
    let stdb = state.stdb.clone();
    let item_for_stdb = item.clone();
    tokio::spawn(async move {
        stdb.persist_item(&item_for_stdb).await;
    });

    let _ = state.tx.send(item.clone());

    let mut items = state.items.write().await;
    items.push(item.clone());

    (StatusCode::CREATED, Json(item))
}

async fn list_items(State(state): State<AppState>) -> impl IntoResponse {
    let items = state.items.read().await;
    let mut sorted: Vec<Item> = items.clone();
    sorted.reverse();
    Json(sorted)
}

async fn get_item(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let items = state.items.read().await;
    match items.iter().find(|i| i.id == id) {
        Some(item) => Ok(Json(item.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // Send full history on connect
    {
        let items = state.items.read().await;
        for item in items.iter() {
            if let Ok(json) = serde_json::to_string(item) {
                if sender.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
        }
    }

    let send_task = tokio::spawn(async move {
        while let Ok(item) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&item) {
                if sender.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            // We don't expect client messages, just keep the connection alive
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

enum TlsSource {
    Provided,
    AutoGenerated { created: bool },
}

struct ResolvedTls {
    cert_path: PathBuf,
    key_path: PathBuf,
    source: TlsSource,
}

fn resolve_tls(cert: Option<&str>, key: Option<&str>, bind: &str) -> Result<ResolvedTls, String> {
    match (cert, key) {
        (Some(cert_path), Some(key_path)) => Ok(ResolvedTls {
            cert_path: PathBuf::from(cert_path),
            key_path: PathBuf::from(key_path),
            source: TlsSource::Provided,
        }),
        (None, None) => ensure_auto_tls(bind),
        _ => Err(
            "both --cert and --key must be provided together, or omit both to auto-generate a self-signed certificate".to_string(),
        ),
    }
}

fn ensure_auto_tls(bind: &str) -> Result<ResolvedTls, String> {
    let tls_dir = default_tls_dir();
    create_private_dir_all(&tls_dir)?;

    let cert_path = tls_dir.join("cert.pem");
    let key_path = tls_dir.join("key.pem");
    let cert_exists = cert_path.is_file();
    let key_exists = key_path.is_file();

    if cert_exists && key_exists {
        return Ok(ResolvedTls {
            cert_path,
            key_path,
            source: TlsSource::AutoGenerated { created: false },
        });
    }

    let certified_key = generate_simple_self_signed(tls_subject_alt_names(bind))
        .map_err(|error| format!("failed to generate self-signed TLS certificate: {error}"))?;

    write_private_file(&cert_path, &certified_key.cert.pem())?;
    write_private_file(&key_path, &certified_key.signing_key.serialize_pem())?;

    Ok(ResolvedTls {
        cert_path,
        key_path,
        source: TlsSource::AutoGenerated { created: true },
    })
}

fn tls_subject_alt_names(bind: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    names.insert("localhost".to_string());
    names.insert("127.0.0.1".to_string());
    names.insert("::1".to_string());

    if let Some(host) = bind_host(bind) {
        if !host.is_empty() && host != "0.0.0.0" && host != "::" {
            names.insert(host);
        }
    }

    names.into_iter().collect()
}

fn bind_host(bind: &str) -> Option<String> {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return Some(addr.ip().to_string());
    }

    if let Some(stripped) = bind.strip_prefix('[') {
        if let Some((host, _)) = stripped.split_once("]:") {
            return Some(host.to_string());
        }
    }

    bind.rsplit_once(':').map(|(host, _)| host.to_string())
}

fn default_tls_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AGENT_DISPLAY_TLS_DIR") {
        return PathBuf::from(dir);
    }

    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("llm-viewer-server").join("tls");
    }

    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("llm-viewer-server")
            .join("tls");
    }

    std::env::temp_dir().join("llm-viewer-server").join("tls")
}

fn create_private_dir_all(path: &std::path::Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder.create(path).map_err(|error| {
            format!("failed to create TLS directory {}: {error}", path.display())
        })?;
    }

    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|error| {
            format!("failed to create TLS directory {}: {error}", path.display())
        })?;
    }

    Ok(())
}

fn write_private_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;

    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("failed to set permissions on {}: {error}", path.display()))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let tls = match resolve_tls(cli.cert.as_deref(), cli.key.as_deref(), &cli.bind) {
        Ok(tls) => tls,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };

    let api_key = resolve_api_key(cli.api_key.clone());
    let stdb = StdbClient::new(cli.stdb_server.clone(), cli.stdb_database.clone());

    // Load history from SpacetimeDB
    let history = stdb.load_history().await;
    let history_count = history.len();
    if history_count > 0 {
        println!("Loaded {history_count} items from SpacetimeDB");
    }

    let (tx, _) = broadcast::channel::<Item>(100);

    let state = AppState {
        items: Arc::new(RwLock::new(history)),
        tx,
        api_key: api_key.clone(),
        stdb,
    };

    let app = Router::new()
        .route("/push", post(push_item))
        .route("/items", get(list_items))
        .route("/items/{id}", get(get_item))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    match &tls.source {
        TlsSource::Provided => {
            println!(
                "Using configured TLS certificate {}",
                tls.cert_path.display()
            );
        }
        TlsSource::AutoGenerated { created: true } => {
            println!(
                "Generated self-signed TLS certificate at {}",
                tls.cert_path.display()
            );
        }
        TlsSource::AutoGenerated { created: false } => {
            println!(
                "Reusing self-signed TLS certificate at {}",
                tls.cert_path.display()
            );
        }
    }

    let tls_config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_path, &tls.key_path)
            .await
            .expect("Failed to load TLS cert/key");

    let addr: SocketAddr = cli.bind.parse().expect("Invalid bind address");
    println!("LLM Viewer Server listening on https://{}", addr);
    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
