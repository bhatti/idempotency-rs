[![Crates.io](https://img.shields.io/crates/v/idempotency-rs.svg)](https://crates.io/crates/idempotency-rs)
[![Documentation](https://docs.rs/idempotency-rs/badge.svg)](https://docs.rs/idempotency-rs)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/yourusername/idempotency-rs#license)
[![Build Status](https://github.com/yourusername/idempotency-rs/workflows/CI/badge.svg)](https://github.com/yourusername/idempotency-rs/actions)

A sample idempotency middleware for Rust web services, following battle-tested patterns from Stripe and AWS.

## Why This Matters

Idempotency failures cause duplicate transactions, data corruption, and customer trust issues. Most implementations get it wrong by conflating "duplicate detection" with true idempotency, creating dangerous race conditions.

This library implements the correct atomic patterns to ensure your APIs can be safely retried.

## Features

- **Client-driven idempotency keys** with UUID v4 validation
- **Request fingerprinting** to prevent key reuse with different payloads  
- **Atomic lock acquisition** to eliminate race conditions
- **Concurrent request handling** with proper 409 Conflict responses
- **Framework integrations**: Axum, Actix-Web, Tonic/gRPC
- **24-hour key retention** with automatic cleanup
- **Response caching** for both successes and deterministic failures

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
idempotency-rs = { version = "1.0", features = ["sqlite", "axum-integration"] }
```

### Basic Usage

```rust
use idempotency_rs::{IdempotencyMiddleware, SqliteIdempotencyStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct CreateOrderRequest {
    symbol: String,
    quantity: u32,
    price: f64,
}

#[derive(Serialize)]
struct CreateOrderResponse {
    order_id: String,
    status: String,
    total: f64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize store
    let store = SqliteIdempotencyStore::new("sqlite:idempotency.db").await?;
    let middleware = IdempotencyMiddleware::new(store);

    let request = CreateOrderRequest {
        symbol: "AAPL".to_string(),
        quantity: 100,
        price: 150.0,
    };

    // Process with idempotency protection
    let result = middleware.process_request(
        Some(Uuid::new_v4().to_string()),  // Client-generated key
        "user123".to_string(),             // User/tenant scoping
        "/api/orders".to_string(),         // Request path
        "POST".to_string(),                // HTTP method
        &request,                          // Request payload
        || async {                         // Your business logic
            // Simulate order processing
            let order_id = Uuid::new_v4().to_string();
            let response = CreateOrderResponse {
                order_id,
                status: "pending".to_string(),
                total: 15000.0,
            };
            
            Ok((200, HashMap::new(), response))
        },
    ).await?;

    println!("Response: {:?}", result);
    Ok(())
}
```

### Axum Integration

```rust
use axum::{Router, routing::post, extract::State, Json};
use idempotency_rs::axum_integration::idempotency_layer;
use std::sync::Arc;

async fn create_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<Json<CreateOrderResponse>, StatusCode> {
    // Your business logic here
    let response = CreateOrderResponse {
        order_id: Uuid::new_v4().to_string(),
        status: "pending".to_string(),
        total: req.quantity as f64 * req.price,
    };
    
    Ok(Json(response))
}

struct AppState {
    idempotency_store: SqliteIdempotencyStore,
}

#[tokio::main]
async fn main() {
    let store = SqliteIdempotencyStore::new("sqlite:idempotency.db")
        .await
        .expect("Failed to create store");
    
    let state = Arc::new(AppState {
        idempotency_store: store,
    });

    let app = Router::new()
        .route("/orders", post(create_order))
        .layer(idempotency_layer(state.clone()))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

## Testing Idempotency

Start the example server:

```bash
cargo run --example axum_server --features axum-integration
```

Test with curl:

```bash
# Generate a UUID for testing
KEY=$(uuidgen)

# First request
curl -X POST http://localhost:3000/orders \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"AAPL","quantity":100,"price":150.0}'

# Retry with same key - returns identical response
curl -X POST http://localhost:3000/orders \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"AAPL","quantity":100,"price":150.0}'

# Different payload with same key - returns 422 error
curl -X POST http://localhost:3000/orders \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: $KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"GOOGL","quantity":50,"price":2000.0}'
```

Or run the automated test suite:

```bash
# Run all tests
make test

# Run integration tests specifically
cargo test --test integration_tests

# Run with coverage
make coverage
```

## Storage Backends

### SQLite (Default)

Perfect for single-instance deployments and development:

```rust
let store = SqliteIdempotencyStore::new("sqlite:./idempotency.db").await?;
```


### Custom Configuration

```rust
let middleware = IdempotencyMiddleware::with_config(
    store,
    Duration::hours(24),        // Key retention (TTL)
    Duration::seconds(30),      // Lock timeout
);
```

## License
- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

