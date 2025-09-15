//! gRPC integration for idempotency middleware

use tonic::{Request, Response, Status};
use crate::{IdempotencyMiddleware, IdempotencyStore};
use std::sync::Arc;

pub struct IdempotencyInterceptor<S: IdempotencyStore> {
    middleware: Arc<IdempotencyMiddleware<S>>,
}

impl<S: IdempotencyStore> IdempotencyInterceptor<S> {
    pub fn new(store: S) -> Self {
        Self {
            middleware: Arc::new(IdempotencyMiddleware::new(store)),
        }
    }
    
    pub async fn intercept<T, U>(
        &self,
        mut request: Request<T>,
        handler: impl FnOnce(Request<T>) -> F,
    ) -> Result<Response<U>, Status>
    where
        T: serde::Serialize,
        U: serde::Serialize + for<'de> serde::Deserialize<'de>,
        F: std::future::Future<Output = Result<Response<U>, Status>>,
    {
        // Extract idempotency key from metadata
        let idempotency_key = request
            .metadata()
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        
        if idempotency_key.is_none() {
            return Err(Status::invalid_argument("Missing idempotency-key in metadata"));
        }
        
        // Extract user ID from metadata
        let user_id = request
            .metadata()
            .get("user-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("anonymous")
            .to_string();
        
        // Get request path from extensions
        let path = request.uri().path().to_string();
        let method = "UNARY".to_string(); // For gRPC unary calls
        
        let request_body = request.get_ref();
        
        // Process with idempotency
        match self.middleware.process_request(
            idempotency_key,
            user_id,
            path,
            method,
            request_body,
            || handler(request),
        ).await {
            Ok(cached_response) => {
                // Deserialize cached response
                let response: U = serde_json::from_slice(&cached_response.body)
                    .map_err(|e| Status::internal(format!("Failed to deserialize cached response: {}", e)))?;
                Ok(Response::new(response))
            }
            Err(e) => {
                match e {
                    crate::IdempotencyError::RequestInProgress { retry_after } => {
                        Err(Status::aborted(format!("Request in progress, retry after {} seconds", retry_after)))
                    }
                    crate::IdempotencyError::KeyReusedWithDifferentRequest => {
                        Err(Status::failed_precondition("Idempotency key reused with different request"))
                    }
                    _ => Err(Status::internal(e.to_string())),
                }
            }
        }
    }
}

// Example proto file content for reference
pub const EXAMPLE_PROTO: &str = r#"
syntax = "proto3";

package orders.v1;

service OrderService {
    rpc CreateOrder(CreateOrderRequest) returns (CreateOrderResponse);
}

message CreateOrderRequest {
    // Required: Client-generated idempotency key (UUID v4)
    string idempotency_key = 1;
    
    string symbol = 2;
    uint32 quantity = 3;
    double price = 4;
}

message CreateOrderResponse {
    string order_id = 1;
    string status = 2;
    double total = 3;
}
"#;
