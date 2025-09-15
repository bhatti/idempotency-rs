use axum::{
    async_trait,
    extract::{FromRequestParts, State},
    http::{header, request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Axum middleware for idempotency
pub async fn idempotency_middleware<S>(
    State(store): State<Arc<S>>,
    parts: Parts,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode>
where
    S: IdempotencyStore + 'static,
{
    // Only apply to mutating methods
    let method = parts.method.clone();
    if !matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        return Ok(next.run(req).await);
    }

    // Extract idempotency key from header
    let idempotency_key = parts
        .headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if idempotency_key.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Extract user ID from auth context (simplified)
    let user_id = parts
        .headers
        .get("X-User-ID")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();

    let path = parts.uri.path().to_string();
    let method_str = method.to_string();

    // Process with idempotency
    let middleware = IdempotencyMiddleware::new(store);

    // ... rest of integration

    Ok(next.run(req).await)
}
