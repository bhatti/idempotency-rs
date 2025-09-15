# Second request with same key (should return cached response)
echo "Sending duplicate request..."
RESPONSE2=$(curl -s -X POST http://127.0.0.1:3000/orders \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $IDEMPOTENCY_KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"AAPL","quantity":100,"price":150.0}')

echo "Second response: $RESPONSE2"

# Verify responses are identical
if [ "$RESPONSE1" == "$RESPONSE2" ]; then
    echo "✅ SUCCESS: Responses are identical (idempotency working)"
else
    echo "❌ FAILURE: Responses differ"
    echo "Response 1: $RESPONSE1"
    echo "Response 2: $RESPONSE2"
    kill $SERVER_PID
    exit 1
fi

# Test with different payload but same key (should fail)
echo "Testing key reuse with different payload..."
RESPONSE3=$(curl -s -w "\nHTTP_STATUS:%{http_code}" -X POST http://127.0.0.1:3000/orders \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $IDEMPOTENCY_KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"GOOGL","quantity":50,"price":2000.0}')

if [[ $RESPONSE3 == *"HTTP_STATUS:422"* ]] || [[ $RESPONSE3 == *"KeyReusedWithDifferentRequest"* ]]; then
    echo "✅ SUCCESS: Key reuse with different payload correctly rejected"
else
    echo "❌ FAILURE: Key reuse with different payload not rejected"
    echo "Response: $RESPONSE3"
fi

# Test concurrent requests
echo "Testing concurrent requests..."
NEW_KEY=$(uuidgen)

# Send two requests concurrently
(curl -s -X POST http://127.0.0.1:3000/orders \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $NEW_KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"MSFT","quantity":200,"price":400.0}' > /tmp/concurrent1.txt) &

(curl -s -X POST http://127.0.0.1:3000/orders \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $NEW_KEY" \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"MSFT","quantity":200,"price":400.0}' > /tmp/concurrent2.txt) &

wait

CONCURRENT1=$(cat /tmp/concurrent1.txt)
CONCURRENT2=$(cat /tmp/concurrent2.txt)

echo "Concurrent response 1: $CONCURRENT1"
echo "Concurrent response 2: $CONCURRENT2"

# Test missing idempotency key
echo "Testing missing idempotency key..."
RESPONSE_NO_KEY=$(curl -s -w "\nHTTP_STATUS:%{http_code}" -X POST http://127.0.0.1:3000/orders \
  -H 'Content-Type: application/json' \
  -H "X-User-ID: testuser" \
  -d '{"symbol":"TSLA","quantity":10,"price":800.0}')

if [[ $RESPONSE_NO_KEY == *"HTTP_STATUS:400"* ]]; then
    echo "✅ SUCCESS: Missing idempotency key correctly rejected"
else
    echo "❌ FAILURE: Missing idempotency key not rejected"
fi

# Clean up
kill $SERVER_PID
rm -f /tmp/concurrent*.txt

echo "All tests completed!"
```

```rust:src/grpc_integration.rs
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
```

```yaml:docker-compose.yml
version: '3.8'

services:
  idempotency-service:
    build: .
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=debug
      - DATABASE_URL=sqlite:/app/data/idempotency.db
    volumes:
      - ./data:/app/data
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3000/health"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 40s

  redis:
    image: redis:7-alpine
    ports:
      - "6379:6379"
    volumes:
      - redis-data:/data
    command: redis-server --appendonly yes

  postgres:
    image: postgres:16-alpine
    ports:
      - "5432:5432"
    environment:
      - POSTGRES_USER=idempotency
      - POSTGRES_PASSWORD=idempotency
      - POSTGRES_DB=idempotency
    volumes:
      - postgres-data:/var/lib/postgresql/data
      - ./migrations:/docker-entrypoint-initdb.d

volumes:
  redis-data:
  postgres-data:
```

