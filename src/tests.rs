#[cfg(test)]
mod tests {
    use crate::{
        IdempotencyMiddleware, SqliteIdempotencyStore, IdempotencyStore,
        IdempotencyError, IdempotencyRecord, IdempotencyStatus, LockResult
    };
    use chrono::{Duration, Utc};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
    use uuid::Uuid;

    #[derive(Serialize, Deserialize, Debug, Clone)]
    struct CreateOrderRequest {
        symbol: String,
        quantity: u32,
        price: f64,
    }

    #[derive(Serialize, Deserialize, Debug)]
    struct CreateOrderResponse {
        order_id: String,
        status: String,
        total: f64,
    }

    async fn setup_test_db() -> SqliteIdempotencyStore {
        SqliteIdempotencyStore::new(":memory:")
            .await
            .expect("Failed to create test database")
    }

    #[tokio::test]
    async fn test_successful_request_is_cached() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        let handler = move || {
            let call_count = call_count_clone.clone();
            async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                
                let mut headers = HashMap::new();
                headers.insert("X-Order-ID".to_string(), "order123".to_string());

                let response = CreateOrderResponse {
                    order_id: "order123".to_string(),
                    status: "pending".to_string(),
                    total: 15000.0,
                };

                Ok((200, headers, response))
            }
        };

        // First request - should execute handler
        let result1 = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            handler,
        ).await.unwrap();

        assert_eq!(result1.status_code, 200);
        assert_eq!(result1.headers.get("X-Order-ID").unwrap(), "order123");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // Second request with same key - should return cached response without calling handler
        let result2 = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async {
                panic!("Handler should not be called for cached request");
            },
        ).await.unwrap();

        // Should return exact same response
        assert_eq!(result1.status_code, result2.status_code);
        assert_eq!(result1.body, result2.body);
        assert_eq!(result1.headers, result2.headers);
        assert_eq!(call_count.load(Ordering::SeqCst), 1); // Handler called only once
    }

    #[tokio::test]
    async fn test_different_fingerprint_rejected() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();

        let request1 = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        // First request
        let _ = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request1,
            || async { Ok((200, HashMap::new(), CreateOrderResponse {
                order_id: "order123".to_string(),
                status: "pending".to_string(),
                total: 15000.0,
            }))},
        ).await.unwrap();

        // Second request with same key but different payload
        let request2 = CreateOrderRequest {
            symbol: "GOOGL".to_string(),  // Different!
            quantity: 100,
            price: 150.0,
        };

        let result = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request2,
            || async { panic!("Should not be called") },
        ).await;

        assert!(matches!(result, Err(IdempotencyError::KeyReusedWithDifferentRequest)));
    }

    #[tokio::test]
    async fn test_concurrent_requests_handled() {
        let store = setup_test_db().await;
        let middleware = Arc::new(IdempotencyMiddleware::new(store));

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        let call_count = Arc::new(AtomicU32::new(0));

        // Create two concurrent requests with the same idempotency key
        let middleware1 = middleware.clone();
        let key1 = key.clone();
        let user_id1 = user_id.clone();
        let request1 = request.clone();
        let call_count1 = call_count.clone();

        let handle1 = tokio::spawn(async move {
            middleware1.process_request(
                Some(key1),
                user_id1,
                "/orders".to_string(),
                "POST".to_string(),
                &request1,
                || {
                    let call_count = call_count1.clone();
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        // Simulate processing time
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        Ok((200, HashMap::new(), CreateOrderResponse {
                            order_id: "order123".to_string(),
                            status: "pending".to_string(),
                            total: 15000.0,
                        }))
                    }
                },
            ).await
        });

        // Small delay to ensure first request starts processing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let middleware2 = middleware.clone();
        let key2 = key.clone();
        let user_id2 = user_id.clone();
        let request2 = request.clone();

        let handle2 = tokio::spawn(async move {
            middleware2.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
                Some(key2),
                user_id2,
                "/orders".to_string(),
                "POST".to_string(),
                &request2,
                || async {
                    panic!("Second handler should not be called due to lock");
                },
            ).await
        });

        // Wait for both requests to complete
        let result1 = handle1.await.unwrap();
        let result2 = handle2.await.unwrap();

        // First request should succeed
        assert!(result1.is_ok());

        // Second request should get "request in progress" error
        assert!(matches!(result2, Err(IdempotencyError::RequestInProgress { .. })));

        // Handler should only be called once
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_server_errors_are_retryable() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        // First request - handler returns 500 error
        let result1 = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            move || {
                let call_count = call_count_clone.clone();
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok((500, HashMap::new(), CreateOrderResponse {
                        order_id: "error".to_string(),
                        status: "error".to_string(),
                        total: 0.0,
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
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            move || {
                let call_count = call_count_clone2.clone();
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok((200, HashMap::new(), CreateOrderResponse {
                        order_id: "order123".to_string(),
                        status: "success".to_string(),
                        total: 15000.0,
                    }))
                }
            },
        ).await.unwrap();

        assert_eq!(result2.status_code, 200);
        assert_eq!(call_count.load(Ordering::SeqCst), 2); // Handler called twice
    }

    #[tokio::test]
    async fn test_client_errors_not_retryable() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "INVALID".to_string(),
            quantity: 0,  // Invalid quantity
            price: -100.0,  // Invalid price
        };

        // First request - handler returns 400 error (client error)
        let result1 = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { Ok((400, HashMap::new(), CreateOrderResponse {
                order_id: "error".to_string(),
                status: "invalid_request".to_string(),
                total: 0.0,
            }))},
        ).await.unwrap();

        assert_eq!(result1.status_code, 400);

        // Second request - should return cached error response (not retryable)
        let result2 = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async {
                panic!("Handler should not be called for cached permanent failure");
            },
        ).await.unwrap();

        // Should return the same error response
        assert_eq!(result1.body, result2.body);
        assert_eq!(result2.status_code, 400);
    }

    #[tokio::test]
    async fn test_missing_idempotency_key_rejected() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        let result = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            None,  // No idempotency key
            "user123".to_string(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { panic!("Should not be called") },
        ).await;

        assert!(matches!(result, Err(IdempotencyError::MissingIdempotencyKey)));
    }

    #[tokio::test]
    async fn test_invalid_key_format_rejected() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        let result = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            Some("not-a-uuid".to_string()),  // Invalid format
            "user123".to_string(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { panic!("Should not be called") },
        ).await;

        assert!(matches!(result, Err(IdempotencyError::InvalidKeyFormat)));
    }

    #[tokio::test]
    async fn test_user_scoped_keys() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        // User 1 makes a request
        let result1 = middleware.process_request(
            Some(key.clone()),
            "user1".to_string(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { Ok((200, HashMap::new(), CreateOrderResponse {
                order_id: "order1".to_string(),
                status: "pending".to_string(),
                total: 15000.0,
            }))},
        ).await.unwrap();

        // User 2 makes request with SAME key - should be allowed
        let result2 = middleware.process_request(
            Some(key.clone()),
            "user2".to_string(),  // Different user
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { Ok((200, HashMap::new(), CreateOrderResponse {
                order_id: "order2".to_string(),  // Different order ID
                status: "pending".to_string(),
                total: 15000.0,
            }))},
        ).await.unwrap();

        // Different responses for different users
        assert_ne!(result1.body, result2.body);
    }

    #[tokio::test]
    async fn test_cleanup_expired_records() {
        let store = setup_test_db().await;

        // Insert some test records directly using the store interface
        let past = Utc::now() - Duration::hours(25);  // Expired
        let future = Utc::now() + Duration::hours(1);  // Not expired

        let expired_record = IdempotencyRecord {
            key: Uuid::new_v4().to_string(),
            user_id: "user1".to_string(),
            request_path: "/test".to_string(),
            request_method: "POST".to_string(),
            request_fingerprint: "hash1".to_string(),
            status: IdempotencyStatus::Completed,
            response: None,
            created_at: past,
            expires_at: past + Duration::hours(24),
            locked_until: None,
        };

        let valid_record = IdempotencyRecord {
            key: Uuid::new_v4().to_string(),
            user_id: "user2".to_string(),
            request_path: "/test".to_string(),
            request_method: "POST".to_string(),
            request_fingerprint: "hash2".to_string(),
            status: IdempotencyStatus::Completed,
            response: None,
            created_at: Utc::now(),
            expires_at: future,
            locked_until: None,
        };

        // We'll need to use the try_acquire_lock method since that's our only public insert method
        let _ = store.try_acquire_lock(expired_record.clone()).await.unwrap();
        store.complete_with_response(
            &expired_record.key, 
            &expired_record.user_id, 
            IdempotencyStatus::Completed, 
            None
        ).await.unwrap();

        let _ = store.try_acquire_lock(valid_record.clone()).await.unwrap();
        store.complete_with_response(
            &valid_record.key, 
            &valid_record.user_id, 
            IdempotencyStatus::Completed, 
            None
        ).await.unwrap();

        // Run cleanup
        let deleted = store.cleanup_expired().await.unwrap();
        assert_eq!(deleted, 1);

        // Expired record should be gone
        let result = store.get(&expired_record.key, &expired_record.user_id).await.unwrap();
        assert!(result.is_none());

        // Valid record should still exist
        let result = store.get(&valid_record.key, &valid_record.user_id).await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_handler_failure_allows_retry() {
        let store = setup_test_db().await;
        let middleware = IdempotencyMiddleware::new(store);

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        // First request - handler fails with an error
        let result1 = middleware.process_request::<CreateOrderRequest, CreateOrderResponse, _, _>(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async {
                Err(IdempotencyError::HandlerFailed("Database connection failed".to_string()))
            },
        ).await;

        assert!(matches!(result1, Err(IdempotencyError::HandlerFailed(_))));

        // Second request - should be allowed to retry since handler failed
        let result2 = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { Ok((200, HashMap::new(), CreateOrderResponse {
                order_id: "order123".to_string(),
                status: "success".to_string(),
                total: 15000.0,
            }))},
        ).await.unwrap();

        assert_eq!(result2.status_code, 200);
    }

    #[tokio::test] 
    async fn test_lock_timeout_recovery() {
        let store = setup_test_db().await;
        // Use a very short lock timeout for testing
        let middleware = IdempotencyMiddleware::with_config(
            store, 
            Duration::hours(24), 
            Duration::milliseconds(50) // Very short timeout
        );

        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        let request = CreateOrderRequest {
            symbol: "AAPL".to_string(),
            quantity: 100,
            price: 150.0,
        };

        // Start a request that will hold the lock for longer than timeout
        let middleware_clone = middleware.clone();
        let key_clone = key.clone();
        let user_id_clone = user_id.clone();
        let request_clone = request.clone();

        let handle1 = tokio::spawn(async move {
            middleware_clone.process_request(
                Some(key_clone),
                user_id_clone,
                "/orders".to_string(),
                "POST".to_string(),
                &request_clone,
                || async {
                    // Sleep longer than lock timeout
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    Ok((200, HashMap::new(), CreateOrderResponse {
                        order_id: "order1".to_string(),
                        status: "pending".to_string(),
                        total: 15000.0,
                    }))
                },
            ).await
        });

        // Wait for lock timeout to expire
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Try another request - should be able to acquire expired lock
        let result2 = middleware.process_request(
            Some(key.clone()),
            user_id.clone(),
            "/orders".to_string(),
            "POST".to_string(),
            &request,
            || async { Ok((200, HashMap::new(), CreateOrderResponse {
                order_id: "order2".to_string(),
                status: "success".to_string(),
                total: 15000.0,
            }))},
        ).await.unwrap();

        assert_eq!(result2.status_code, 200);
        
        // Wait for first request to complete
        let _ = handle1.await;
    }

    #[tokio::test]
    async fn test_atomic_operations() {
        let store = setup_test_db().await;
        
        // Test that try_acquire_lock is truly atomic
        let key = Uuid::new_v4().to_string();
        let user_id = "user123".to_string();
        
        let record = IdempotencyRecord {
            key: key.clone(),
            user_id: user_id.clone(),
            request_path: "/test".to_string(),
            request_method: "POST".to_string(),
            request_fingerprint: "fingerprint1".to_string(),
            status: IdempotencyStatus::Pending,
            response: None,
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::hours(24),
            locked_until: Some(Utc::now() + Duration::seconds(30)),
        };

        // First acquire should succeed
        let result1 = store.try_acquire_lock(record.clone()).await.unwrap();
        assert!(matches!(result1, LockResult::Acquired));

        // Second acquire with same key should detect existing record
        let result2 = store.try_acquire_lock(record.clone()).await.unwrap();
        assert!(matches!(result2, LockResult::InProgress { .. }));

        // Try with different fingerprint should be rejected
        let mut different_record = record.clone();
        different_record.request_fingerprint = "fingerprint2".to_string();
        
        let result3 = store.try_acquire_lock(different_record).await.unwrap();
        assert!(matches!(result3, LockResult::KeyReused));
    }
}
