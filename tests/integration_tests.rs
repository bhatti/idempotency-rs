//! Integration tests for the idempotency middleware

use idempotency_rs::{IdempotencyMiddleware, SqliteIdempotencyStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PaymentRequest {
    amount: f64,
    currency: String,
    recipient: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct PaymentResponse {
    payment_id: String,
    status: String,
    amount: f64,
}

async fn setup_test_store() -> SqliteIdempotencyStore {
    SqliteIdempotencyStore::new(":memory:")
        .await
        .expect("Failed to create test database")
}

#[tokio::test]
async fn test_end_to_end_payment_flow() {
    // Create in-memory database for testing
    let store = setup_test_store().await;
    let middleware = IdempotencyMiddleware::new(store);

    let key = Uuid::new_v4().to_string();
    let user_id = "user123".to_string();

    let request = PaymentRequest {
        amount: 100.0,
        currency: "USD".to_string(),
        recipient: "recipient@example.com".to_string(),
    };

    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    // Simulate payment processing
    let payment_processor = move || {
        let call_count = call_count_clone.clone();
        async move {
            call_count.fetch_add(1, Ordering::SeqCst);
            
            // Simulate processing time
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            let response = PaymentResponse {
                payment_id: Uuid::new_v4().to_string(),
                status: "completed".to_string(),
                amount: 100.0,
            };

            Ok((200, HashMap::new(), response))
        }
    };

    // First request
    let result1 = middleware.process_request(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        payment_processor,
    ).await.unwrap();

    assert_eq!(result1.status_code, 200);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    // Parse response
    let response1: PaymentResponse = serde_json::from_slice(&result1.body).unwrap();
    assert_eq!(response1.status, "completed");

    // Retry with same key - should return cached response
    let result2 = middleware.process_request::<PaymentRequest, PaymentResponse, _, _>(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        || async { panic!("Should not be called") },
    ).await.unwrap();

    assert_eq!(result2.status_code, 200);
    let response2: PaymentResponse = serde_json::from_slice(&result2.body).unwrap();

    // Verify exact same payment_id (proving it's cached)
    assert_eq!(response1.payment_id, response2.payment_id);
    
    // Verify handler was only called once
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_stress_concurrent_requests() {
    let store = setup_test_store().await;
    let middleware = Arc::new(IdempotencyMiddleware::new(store));

    let num_concurrent_requests = 100;
    let num_unique_keys = 10;

    // Pre-generate valid UUID keys that will be reused
    let unique_keys: Vec<String> = (0..num_unique_keys)
        .map(|_| Uuid::new_v4().to_string())
        .collect();

    let mut handles = vec![];
    let call_count = Arc::new(AtomicU32::new(0));

    for i in 0..num_concurrent_requests {
        let middleware = middleware.clone();
        let key = unique_keys[i % num_unique_keys].clone(); // Reuse UUID keys
        let call_count = call_count.clone();

        let handle = tokio::spawn(async move {
            // All requests with the same key must have identical payloads
            // to avoid KeyReusedWithDifferentRequest error
            let key_index = i % num_unique_keys;
            let request = PaymentRequest {
                amount: 100.0,
                currency: "USD".to_string(),
                recipient: format!("recipient{}", key_index), // Same recipient for same key
            };

            middleware.process_request(
                Some(key),
                "user".to_string(),
                "/payments".to_string(),
                "POST".to_string(),
                &request,
                || {
                    let call_count = call_count.clone();
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        Ok((200, HashMap::new(), "response"))
                    }
                },
            ).await
        });

        handles.push(handle);
    }

    let mut successful_requests = 0;
    let mut in_progress_errors = 0;

    // Wait for all requests to complete
    for handle in handles {
        let result = handle.await.unwrap();
        match result {
            Ok(_) => successful_requests += 1,
            Err(idempotency_rs::IdempotencyError::RequestInProgress { .. }) => in_progress_errors += 1,
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // We should have exactly num_unique_keys successful requests
    assert_eq!(successful_requests, num_unique_keys);
    
    // And the rest should be "in progress" errors
    assert_eq!(in_progress_errors, num_concurrent_requests - num_unique_keys);
    
    // Handler should be called exactly num_unique_keys times
    assert_eq!(call_count.load(Ordering::SeqCst), num_unique_keys as u32);
}

#[tokio::test]
async fn test_different_users_same_key() {
    let store = setup_test_store().await;
    let middleware = IdempotencyMiddleware::new(store);

    let key = Uuid::new_v4().to_string();

    let request = PaymentRequest {
        amount: 100.0,
        currency: "USD".to_string(),
        recipient: "recipient@example.com".to_string(),
    };

    // User 1 makes a request
    let result1 = middleware.process_request(
        Some(key.clone()),
        "user1".to_string(),
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        || async {
            Ok((200, HashMap::new(), PaymentResponse {
                payment_id: "payment1".to_string(),
                status: "completed".to_string(),
                amount: 100.0,
            }))
        },
    ).await.unwrap();

    // User 2 makes request with SAME key - should be allowed
    let result2 = middleware.process_request(
        Some(key.clone()),
        "user2".to_string(), // Different user
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        || async {
            Ok((200, HashMap::new(), PaymentResponse {
                payment_id: "payment2".to_string(), // Different payment ID
                status: "completed".to_string(),
                amount: 100.0,
            }))
        },
    ).await.unwrap();

    // Different responses for different users
    assert_ne!(result1.body, result2.body);
    
    let payment1: PaymentResponse = serde_json::from_slice(&result1.body).unwrap();
    let payment2: PaymentResponse = serde_json::from_slice(&result2.body).unwrap();
    
    assert_eq!(payment1.payment_id, "payment1");
    assert_eq!(payment2.payment_id, "payment2");
}

#[tokio::test]
async fn test_request_fingerprint_validation() {
    let store = setup_test_store().await;
    let middleware = IdempotencyMiddleware::new(store);

    let key = Uuid::new_v4().to_string();
    let user_id = "user123".to_string();

    let request1 = PaymentRequest {
        amount: 100.0,
        currency: "USD".to_string(),
        recipient: "recipient@example.com".to_string(),
    };

    // First request
    let _result1 = middleware.process_request(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request1,
        || async {
            Ok((200, HashMap::new(), PaymentResponse {
                payment_id: "payment1".to_string(),
                status: "completed".to_string(),
                amount: 100.0,
            }))
        },
    ).await.unwrap();

    // Second request with same key but different amount (different fingerprint)
    let request2 = PaymentRequest {
        amount: 200.0, // Different amount!
        currency: "USD".to_string(),
        recipient: "recipient@example.com".to_string(),
    };

    let result2 = middleware.process_request::<PaymentRequest, PaymentResponse, _, _>(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request2,
        || async { panic!("Should not be called") },
    ).await;

    // Should be rejected due to different fingerprint
    assert!(matches!(result2, Err(idempotency_rs::IdempotencyError::KeyReusedWithDifferentRequest)));
}

#[tokio::test]
async fn test_error_handling_and_retries() {
    let store = setup_test_store().await;
    let middleware = IdempotencyMiddleware::new(store);

    let key = Uuid::new_v4().to_string();
    let user_id = "user123".to_string();

    let request = PaymentRequest {
        amount: 100.0,
        currency: "USD".to_string(),
        recipient: "recipient@example.com".to_string(),
    };

    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    // First request - simulate a 5xx server error (retryable)
    let result1 = middleware.process_request(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        move || {
            let call_count = call_count_clone.clone();
            async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Ok((500, HashMap::new(), PaymentResponse {
                    payment_id: "error".to_string(),
                    status: "internal_error".to_string(),
                    amount: 0.0,
                }))
            }
        },
    ).await.unwrap();

    assert_eq!(result1.status_code, 500);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);

    // Second request - should be allowed to retry since 5xx is retryable
    let call_count_clone2 = call_count.clone();
    let result2 = middleware.process_request(
        Some(key.clone()),
        user_id.clone(),
        "/payments".to_string(),
        "POST".to_string(),
        &request,
        move || {
            let call_count = call_count_clone2.clone();
            async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Ok((200, HashMap::new(), PaymentResponse {
                    payment_id: "success".to_string(),
                    status: "completed".to_string(),
                    amount: 100.0,
                }))
            }
        },
    ).await.unwrap();

    assert_eq!(result2.status_code, 200);
    // Handler should be called twice (first failure, then success)
    assert_eq!(call_count.load(Ordering::SeqCst), 2);

    let response: PaymentResponse = serde_json::from_slice(&result2.body).unwrap();
    assert_eq!(response.status, "completed");
}
