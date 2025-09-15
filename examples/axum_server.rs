//! Example Axum server with idempotency middleware
//! Run with: cargo run --example axum_server --features axum-integration

#[cfg(feature = "axum-integration")]
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};

#[cfg(feature = "axum-integration")]
use idempotency_rs::{
    IdempotencyMiddleware, SqliteIdempotencyStore,
    axum_integration::idempotency_layer,
};

#[cfg(feature = "axum-integration")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "axum-integration")]
use std::sync::Arc;

#[cfg(feature = "axum-integration")]
use tower_http::trace::TraceLayer;

#[cfg(feature = "axum-integration")]
use uuid::Uuid;

#[cfg(feature = "axum-integration")]
#[derive(Debug, Serialize, Deserialize)]
struct CreateOrderRequest {
    symbol: String,
    quantity: u32,
    price: f64,
}

#[cfg(feature = "axum-integration")]
#[derive(Debug, Serialize, Deserialize)]
struct CreateOrderResponse {
    order_id: String,
    status: String,
    total: f64,
}

#[cfg(feature = "axum-integration")]
async fn create_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Simulate order processing
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let order_id = Uuid::new_v4().to_string();
    let total = req.quantity as f64 * req.price;

    let response = CreateOrderResponse {
        order_id,
        status: "pending".to_string(),
        total,
    };

    Ok(Json(response))
}

#[cfg(feature = "axum-integration")]
struct AppState {
    idempotency_store: SqliteIdempotencyStore,
}

#[cfg(feature = "axum-integration")]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Initialize idempotency store
    let store = SqliteIdempotencyStore::new("sqlite:idempotency.db")
        .await
        .expect("Failed to create idempotency store");

    let state = Arc::new(AppState {
        idempotency_store: store,
    });

    // Build router with middleware
    let app = Router::new()
        .route("/orders", post(create_order))
        .layer(idempotency_layer(state.clone()))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("Server running on http://127.0.0.1:3000");
    println!("Try: curl -X POST http://127.0.0.1:3000/orders \\");
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -H 'Idempotency-Key: $(uuidgen)' \\");
    println!("  -d '{{\"symbol\":\"AAPL\",\"quantity\":100,\"price\":150.0}}'");

    axum::serve(listener, app).await.unwrap();
}

#[cfg(not(feature = "axum-integration"))]
fn main() {
    println!("This example requires the 'axum-integration' feature.");
    println!("Run with: cargo run --example axum_server --features axum-integration");
}
