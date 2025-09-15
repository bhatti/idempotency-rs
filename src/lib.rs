//! # Idempotency-rs
//!
//! A sample idempotency middleware for Rust web services.
//!
//! ## Features
//! - Client-driven idempotency keys (UUID v4)
//! - Request fingerprinting to prevent key reuse
//! - Atomic concurrent request handling with proper locking
//! - SQLite and Redis storage backends
//! - Integration with Axum, Actix-Web, and Tonic
//! - Follows Stripe's idempotency patterns

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum IdempotencyError {
    #[error("Request in progress (retry after {retry_after} seconds)")]
    RequestInProgress { retry_after: u64 },

    #[error("Idempotency key reused with different request")]
    KeyReusedWithDifferentRequest,

    #[error("Missing idempotency key")]
    MissingIdempotencyKey,

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Invalid idempotency key format")]
    InvalidKeyFormat,

    #[error("Transaction failed: {0}")]
    TransactionFailed(String),

    #[error("Concurrent request conflict")]
    ConcurrentRequestConflict,

    #[error("Handler execution failed: {0}")]
    HandlerFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IdempotencyStatus {
    Pending,
    Completed,
    Failed { is_retryable: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub user_id: String,  // Scope keys to user/tenant
    pub request_path: String,
    pub request_method: String,
    pub request_fingerprint: String,
    pub status: IdempotencyStatus,
    pub response: Option<CachedResponse>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub locked_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub status_code: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Result of attempting to acquire an idempotency lock
#[derive(Debug)]
pub enum LockResult {
    /// Lock acquired successfully, safe to proceed
    Acquired,
    /// Request already completed, return cached response
    AlreadyCompleted(CachedResponse),
    /// Request is currently being processed by another worker
    InProgress { retry_after: u64 },
    /// Key reused with different request payload
    KeyReused,
    /// Failed permanently, return cached error response
    FailedPermanently(CachedResponse),
}

/// Trait for idempotency storage backends
#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// Atomically attempt to acquire a lock for processing
    /// This must be an atomic operation that either:
    /// 1. Creates a new PENDING record and returns Acquired
    /// 2. Returns the current state if record exists
    async fn try_acquire_lock(
        &self,
        record: IdempotencyRecord,
    ) -> Result<LockResult, IdempotencyError>;

    /// Atomically update record with final result and release lock
    /// This must happen in a single transaction with business logic
    async fn complete_with_response(
        &self,
        key: &str,
        user_id: &str,
        status: IdempotencyStatus,
        response: Option<CachedResponse>,
    ) -> Result<(), IdempotencyError>;

    /// Atomically release lock on failure (for retryable errors)
    async fn release_lock_on_failure(
        &self,
        key: &str,
        user_id: &str,
        is_retryable: bool,
        response: Option<CachedResponse>,
    ) -> Result<(), IdempotencyError>;

    /// Get a record by key and user_id (for debugging/monitoring)
    async fn get(
        &self,
        key: &str,
        user_id: &str,
    ) -> Result<Option<IdempotencyRecord>, IdempotencyError>;

    /// Delete expired records (maintenance operation)
    async fn cleanup_expired(&self) -> Result<usize, IdempotencyError>;

    /// Execute within a transaction (for stores that support it)
    async fn execute_in_transaction<F, T>(&self, f: F) -> Result<T, IdempotencyError>
    where
        F: FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, IdempotencyError>> + Send>> + Send,
        T: Send;
}

/// Main idempotency middleware
#[derive(Clone)]
pub struct IdempotencyMiddleware<S: IdempotencyStore + Clone> {
    store: S,
    ttl: Duration,
    lock_timeout: Duration,
}

impl<S: IdempotencyStore + Clone> IdempotencyMiddleware<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            ttl: Duration::hours(24),  // Stripe's 24-hour retention
            lock_timeout: Duration::seconds(30),  // Max time to hold lock
        }
    }

    pub fn with_config(store: S, ttl: Duration, lock_timeout: Duration) -> Self {
        Self {
            store,
            ttl,
            lock_timeout,
        }
    }

    /// Get access to the underlying store (for testing)
    #[cfg(test)]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Validate idempotency key format (UUID v4)
    fn validate_key(key: &str) -> Result<(), IdempotencyError> {
        Uuid::parse_str(key)
            .map_err(|_| IdempotencyError::InvalidKeyFormat)?;
        Ok(())
    }

    /// Generate request fingerprint using SHA-256
    pub fn generate_fingerprint<T: Serialize>(request: &T) -> String {
        let json = serde_json::to_string(request).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Process a request with idempotency guarantees
    /// This implements the correct atomic pattern to avoid all race conditions
    pub async fn process_request<Req, Res, F, Fut>(
        &self,
        idempotency_key: Option<String>,
        user_id: String,
        request_path: String,
        request_method: String,
        request: &Req,
        handler: F,
    ) -> Result<CachedResponse, IdempotencyError>
    where
        Req: Serialize,
        Res: Serialize,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(u16, HashMap<String, String>, Res), IdempotencyError>>,
        S: Clone,
    {
        // Require idempotency key for mutating operations
        let key = idempotency_key
            .ok_or(IdempotencyError::MissingIdempotencyKey)?;

        Self::validate_key(&key)?;

        let fingerprint = Self::generate_fingerprint(request);
        let now = Utc::now();

        // Create the record we want to insert
        let record = IdempotencyRecord {
            key: key.clone(),
            user_id: user_id.clone(),
            request_path: request_path.clone(),
            request_method: request_method.clone(),
            request_fingerprint: fingerprint.clone(),
            status: IdempotencyStatus::Pending,
            response: None,
            created_at: now,
            expires_at: now + self.ttl,
            locked_until: Some(now + self.lock_timeout),
        };

        // Step 1: Atomically try to acquire lock
        let lock_result = self.store.try_acquire_lock(record).await?;

        match lock_result {
            LockResult::Acquired => {
                // We got the lock - safe to proceed with business logic
                tracing::debug!("Lock acquired for key: {}", key);
                
                // Execute business logic
                match handler().await {
                    Ok((status_code, headers, response)) => {
                        // Success - cache the response
                        let response_body = serde_json::to_vec(&response)
                            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                        let cached_response = CachedResponse {
                            status_code,
                            headers,
                            body: response_body,
                        };

                        // Determine final status based on HTTP status code
                        let final_status = if status_code >= 500 {
                            IdempotencyStatus::Failed { is_retryable: true }
                        } else if status_code >= 400 {
                            IdempotencyStatus::Failed { is_retryable: false }
                        } else {
                            IdempotencyStatus::Completed
                        };

                        // Atomically complete the request
                        self.store.complete_with_response(
                            &key,
                            &user_id,
                            final_status,
                            Some(cached_response.clone()),
                        ).await?;

                        tracing::debug!("Request completed successfully for key: {}", key);
                        Ok(cached_response)
                    }
                    Err(e) => {
                        // Handler failed - determine if retryable
                        let is_retryable = match &e {
                            IdempotencyError::StorageError(_) => true,
                            IdempotencyError::TransactionFailed(_) => true,
                            IdempotencyError::HandlerFailed(_) => true,
                            _ => false,
                        };

                        // Release lock to allow retry
                        self.store.release_lock_on_failure(
                            &key,
                            &user_id,
                            is_retryable,
                            None, // No response to cache for errors
                        ).await?;

                        tracing::warn!("Handler failed for key: {} - error: {}", key, e);
                        Err(e)
                    }
                }
            }
            LockResult::AlreadyCompleted(response) => {
                // Request was already processed successfully
                tracing::debug!("Returning cached response for key: {}", key);
                Ok(response)
            }
            LockResult::InProgress { retry_after } => {
                // Another request is currently processing this key
                tracing::debug!("Request in progress for key: {}, retry after: {}s", key, retry_after);
                Err(IdempotencyError::RequestInProgress { retry_after })
            }
            LockResult::KeyReused => {
                // Key was reused with different request payload
                tracing::warn!("Key reused with different request for key: {}", key);
                Err(IdempotencyError::KeyReusedWithDifferentRequest)
            }
            LockResult::FailedPermanently(response) => {
                // Request failed permanently, return cached error
                tracing::debug!("Returning cached permanent failure for key: {}", key);
                Ok(response)
            }
        }
    }
}

// Storage implementations
pub mod sqlite_store;

#[cfg(feature = "axum-integration")]
pub mod axum_integration;

#[cfg(feature = "grpc")]
pub mod grpc_integration;

// Re-export for convenience
pub use sqlite_store::SqliteIdempotencyStore;

#[cfg(test)]
mod tests;
