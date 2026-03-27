use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

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

#[derive(Debug, Deserialize)]
pub struct PushRequest {
    pub title: String,
    pub content: String,
    pub content_type: ContentType,
    pub source: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Clone)]
struct AppState {
    items: Arc<RwLock<Vec<Item>>>,
    tx: broadcast::Sender<Item>,
}

#[tokio::main]
async fn main() {
    let (tx, _) = broadcast::channel::<Item>(100);

    let state = AppState {
        items: Arc::new(RwLock::new(Vec::new())),
        tx,
    };

    let app = Router::new()
        .route("/push", post(push_item))
        .route("/items", get(list_items))
        .route("/items/{id}", get(get_item))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3080")
        .await
        .expect("Failed to bind to 127.0.0.1:3080");

    println!("LLM Viewer Server listening on http://127.0.0.1:3080");
    axum::serve(listener, app).await.unwrap();
}

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

async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let items = state.items.read().await;
    match items.iter().find(|i| i.id == id) {
        Some(item) => Ok(Json(item.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

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
