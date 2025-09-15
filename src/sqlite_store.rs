use crate::{
    IdempotencyError, IdempotencyRecord, IdempotencyStatus, IdempotencyStore, 
    CachedResponse, LockResult
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Pool, Sqlite, SqlitePool, Row};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct SqliteIdempotencyStore {
    pool: Pool<Sqlite>,
    // In-memory lock for the entire store to ensure atomicity
    transaction_lock: Arc<Mutex<()>>,
}

impl SqliteIdempotencyStore {
    pub async fn new(database_url: &str) -> Result<Self, IdempotencyError> {
        let pool = SqlitePool::connect(database_url)
            .await
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        // Create tables with proper indexes
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS idempotency_records (
                key TEXT NOT NULL,
                user_id TEXT NOT NULL,
                request_path TEXT NOT NULL,
                request_method TEXT NOT NULL,
                request_fingerprint TEXT NOT NULL,
                status TEXT NOT NULL,
                response_status_code INTEGER,
                response_headers TEXT,
                response_body BLOB,
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                locked_until TEXT,
                PRIMARY KEY (key, user_id)
            );

            CREATE INDEX IF NOT EXISTS idx_expires_at ON idempotency_records(expires_at);
            CREATE INDEX IF NOT EXISTS idx_user_id ON idempotency_records(user_id);
            CREATE INDEX IF NOT EXISTS idx_locked_until ON idempotency_records(locked_until);
            "#
        )
        .execute(&pool)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        Ok(Self { 
            pool,
            transaction_lock: Arc::new(Mutex::new(())),
        })
    }

    fn serialize_status(status: &IdempotencyStatus) -> String {
        match status {
            IdempotencyStatus::Pending => "pending".to_string(),
            IdempotencyStatus::Completed => "completed".to_string(),
            IdempotencyStatus::Failed { is_retryable } => {
                format!("failed:{}", if *is_retryable { "retryable" } else { "permanent" })
            }
        }
    }

    fn deserialize_status(status: &str) -> IdempotencyStatus {
        match status {
            "pending" => IdempotencyStatus::Pending,
            "completed" => IdempotencyStatus::Completed,
            "failed:retryable" => IdempotencyStatus::Failed { is_retryable: true },
            "failed:permanent" => IdempotencyStatus::Failed { is_retryable: false },
            _ => IdempotencyStatus::Pending,
        }
    }

    async fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<IdempotencyRecord, IdempotencyError> {
        let status_str: String = row.try_get("status")
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
        let status = Self::deserialize_status(&status_str);

        let response = if let Some(status_code) = row.try_get::<Option<i32>, _>("response_status_code")
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))? 
        {
            let headers_json: Option<String> = row.try_get("response_headers")
                .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
            let headers = headers_json
                .and_then(|h| serde_json::from_str(&h).ok())
                .unwrap_or_default();

            let body: Option<Vec<u8>> = row.try_get("response_body")
                .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

            Some(CachedResponse {
                status_code: status_code as u16,
                headers,
                body: body.unwrap_or_default(),
            })
        } else {
            None
        };

        Ok(IdempotencyRecord {
            key: row.try_get("key").map_err(|e| IdempotencyError::StorageError(e.to_string()))?,
            user_id: row.try_get("user_id").map_err(|e| IdempotencyError::StorageError(e.to_string()))?,
            request_path: row.try_get("request_path").map_err(|e| IdempotencyError::StorageError(e.to_string()))?,
            request_method: row.try_get("request_method").map_err(|e| IdempotencyError::StorageError(e.to_string()))?,
            request_fingerprint: row.try_get("request_fingerprint").map_err(|e| IdempotencyError::StorageError(e.to_string()))?,
            status,
            response,
            created_at: {
                let dt_str: String = row.try_get("created_at").map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
                DateTime::parse_from_rfc3339(&dt_str)
                    .map_err(|e| IdempotencyError::StorageError(e.to_string()))?
                    .with_timezone(&Utc)
            },
            expires_at: {
                let dt_str: String = row.try_get("expires_at").map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
                DateTime::parse_from_rfc3339(&dt_str)
                    .map_err(|e| IdempotencyError::StorageError(e.to_string()))?
                    .with_timezone(&Utc)
            },
            locked_until: {
                let dt_str: Option<String> = row.try_get("locked_until").map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
                dt_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc))
            },
        })
    }
}

#[async_trait]
impl IdempotencyStore for SqliteIdempotencyStore {
    /// Atomically attempt to acquire a lock for processing
    async fn try_acquire_lock(
        &self,
        record: IdempotencyRecord,
    ) -> Result<LockResult, IdempotencyError> {
        // Use a global lock to ensure atomicity (in production, rely on DB transactions)
        let _lock = self.transaction_lock.lock().await;
        
        let mut tx = self.pool.begin()
            .await
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        let now = Utc::now();

        // First, check if record exists
        let existing_row = sqlx::query(
            r#"
            SELECT key, user_id, request_path, request_method,
                   request_fingerprint, status, response_status_code,
                   response_headers, response_body, created_at,
                   expires_at, locked_until
            FROM idempotency_records
            WHERE key = ? AND user_id = ?
            "#
        )
        .bind(&record.key)
        .bind(&record.user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        let result = if let Some(row) = existing_row {
            let existing = Self::record_from_row(&row).await?;
            
            // Check fingerprint match
            if existing.request_fingerprint != record.request_fingerprint {
                Ok(LockResult::KeyReused)
            } else {
                // Check current status and lock
                match existing.status {
                    IdempotencyStatus::Completed => {
                        if let Some(response) = existing.response {
                            Ok(LockResult::AlreadyCompleted(response))
                        } else {
                            // If completed but no response, need to reprocess
                            // Update existing record to pending with new lock
                            sqlx::query(
                                r#"
                                UPDATE idempotency_records
                                SET status = ?, locked_until = ?, created_at = ?
                                WHERE key = ? AND user_id = ?
                                "#
                            )
                            .bind(Self::serialize_status(&IdempotencyStatus::Pending))
                            .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
                            .bind(record.created_at.to_rfc3339())
                            .bind(&record.key)
                            .bind(&record.user_id)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                            Ok(LockResult::Acquired)
                        }
                    }
                    IdempotencyStatus::Failed { is_retryable: false } => {
                        if let Some(response) = existing.response {
                            Ok(LockResult::FailedPermanently(response))
                        } else {
                            // If failed but no response, need to reprocess
                            // Update existing record to pending with new lock
                            sqlx::query(
                                r#"
                                UPDATE idempotency_records
                                SET status = ?, locked_until = ?, created_at = ?
                                WHERE key = ? AND user_id = ?
                                "#
                            )
                            .bind(Self::serialize_status(&IdempotencyStatus::Pending))
                            .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
                            .bind(record.created_at.to_rfc3339())
                            .bind(&record.key)
                            .bind(&record.user_id)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                            Ok(LockResult::Acquired)
                        }
                    }
                    IdempotencyStatus::Failed { is_retryable: true } => {
                        // Allow retry for retryable failures
                        // Update existing record to pending with new lock
                        sqlx::query(
                            r#"
                            UPDATE idempotency_records
                            SET status = ?, locked_until = ?, created_at = ?
                            WHERE key = ? AND user_id = ?
                            "#
                        )
                        .bind(Self::serialize_status(&IdempotencyStatus::Pending))
                        .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
                        .bind(record.created_at.to_rfc3339())
                        .bind(&record.key)
                        .bind(&record.user_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                        Ok(LockResult::Acquired)
                    }
                    IdempotencyStatus::Pending => {
                        // Check if lock is still active
                        if let Some(locked_until) = existing.locked_until {
                            if locked_until > now {
                                let retry_after = (locked_until - now).num_seconds() as u64;
                                Ok(LockResult::InProgress { retry_after })
                            } else {
                                // Lock expired, allow reprocessing
                                // Update existing record to pending with new lock
                                sqlx::query(
                                    r#"
                                    UPDATE idempotency_records
                                    SET status = ?, locked_until = ?, created_at = ?
                                    WHERE key = ? AND user_id = ?
                                    "#
                                )
                                .bind(Self::serialize_status(&IdempotencyStatus::Pending))
                                .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
                                .bind(record.created_at.to_rfc3339())
                                .bind(&record.key)
                                .bind(&record.user_id)
                                .execute(&mut *tx)
                                .await
                                .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                                Ok(LockResult::Acquired)
                            }
                        } else {
                            // No lock timeout, allow reprocessing
                            // Update existing record to pending with new lock
                            sqlx::query(
                                r#"
                                UPDATE idempotency_records
                                SET status = ?, locked_until = ?, created_at = ?
                                WHERE key = ? AND user_id = ?
                                "#
                            )
                            .bind(Self::serialize_status(&IdempotencyStatus::Pending))
                            .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
                            .bind(record.created_at.to_rfc3339())
                            .bind(&record.key)
                            .bind(&record.user_id)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

                            Ok(LockResult::Acquired)
                        }
                    }
                }
            }
        } else {
            // Insert new record
            let status = Self::serialize_status(&record.status);
            let headers_json = record.response.as_ref()
                .map(|r| serde_json::to_string(&r.headers).unwrap_or_default());

            sqlx::query(
                r#"
                INSERT INTO idempotency_records (
                    key, user_id, request_path, request_method,
                    request_fingerprint, status, response_status_code,
                    response_headers, response_body, created_at,
                    expires_at, locked_until
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#
            )
            .bind(&record.key)
            .bind(&record.user_id)
            .bind(&record.request_path)
            .bind(&record.request_method)
            .bind(&record.request_fingerprint)
            .bind(status)
            .bind(record.response.as_ref().map(|r| r.status_code as i32))
            .bind(headers_json)
            .bind(record.response.as_ref().map(|r| r.body.clone()))
            .bind(record.created_at.to_rfc3339())
            .bind(record.expires_at.to_rfc3339())
            .bind(record.locked_until.map(|dt| dt.to_rfc3339()))
            .execute(&mut *tx)
            .await
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

            Ok(LockResult::Acquired)
        };

        // Handle transaction based on result
        match &result {
            Ok(LockResult::KeyReused) | Ok(LockResult::InProgress { .. }) => {
                // These cases don't modify the database, rollback to be safe
                tx.rollback().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
            }
            Ok(LockResult::AlreadyCompleted(_)) | Ok(LockResult::FailedPermanently(_)) => {
                // These cases just read data, rollback to be safe
                tx.rollback().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
            }
            Ok(LockResult::Acquired) => {
                // Successfully acquired lock, commit the changes
                tx.commit().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
            }
            Err(_) => {
                // Error occurred, rollback
                tx.rollback().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
            }
        }

        result
    }

    /// Atomically update record with final result and release lock
    async fn complete_with_response(
        &self,
        key: &str,
        user_id: &str,
        status: IdempotencyStatus,
        response: Option<CachedResponse>,
    ) -> Result<(), IdempotencyError> {
        let _lock = self.transaction_lock.lock().await;
        
        let mut tx = self.pool.begin()
            .await
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        let status_str = Self::serialize_status(&status);
        let headers_json = response.as_ref()
            .map(|r| serde_json::to_string(&r.headers).unwrap_or_default());

        sqlx::query(
            r#"
            UPDATE idempotency_records
            SET status = ?,
                response_status_code = ?,
                response_headers = ?,
                response_body = ?,
                locked_until = NULL
            WHERE key = ? AND user_id = ?
            "#
        )
        .bind(status_str)
        .bind(response.as_ref().map(|r| r.status_code as i32))
        .bind(headers_json)
        .bind(response.as_ref().map(|r| r.body.clone()))
        .bind(key)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        tx.commit().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
        Ok(())
    }

    /// Atomically release lock on failure
    async fn release_lock_on_failure(
        &self,
        key: &str,
        user_id: &str,
        is_retryable: bool,
        response: Option<CachedResponse>,
    ) -> Result<(), IdempotencyError> {
        let _lock = self.transaction_lock.lock().await;
        
        let mut tx = self.pool.begin()
            .await
            .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        let status = IdempotencyStatus::Failed { is_retryable };
        let status_str = Self::serialize_status(&status);
        let headers_json = response.as_ref()
            .map(|r| serde_json::to_string(&r.headers).unwrap_or_default());

        sqlx::query(
            r#"
            UPDATE idempotency_records
            SET status = ?,
                response_status_code = ?,
                response_headers = ?,
                response_body = ?,
                locked_until = NULL
            WHERE key = ? AND user_id = ?
            "#
        )
        .bind(status_str)
        .bind(response.as_ref().map(|r| r.status_code as i32))
        .bind(headers_json)
        .bind(response.as_ref().map(|r| r.body.clone()))
        .bind(key)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        tx.commit().await.map_err(|e| IdempotencyError::StorageError(e.to_string()))?;
        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        user_id: &str,
    ) -> Result<Option<IdempotencyRecord>, IdempotencyError> {
        let row = sqlx::query(
            r#"
            SELECT key, user_id, request_path, request_method,
                   request_fingerprint, status, response_status_code,
                   response_headers, response_body, created_at,
                   expires_at, locked_until
            FROM idempotency_records
            WHERE key = ? AND user_id = ?
            "#
        )
        .bind(key)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        match row {
            Some(row) => Ok(Some(Self::record_from_row(&row).await?)),
            None => Ok(None),
        }
    }

    async fn cleanup_expired(&self) -> Result<usize, IdempotencyError> {
        let now = Utc::now().to_rfc3339();

        let result = sqlx::query(
            "DELETE FROM idempotency_records WHERE expires_at < ?"
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| IdempotencyError::StorageError(e.to_string()))?;

        Ok(result.rows_affected() as usize)
    }

    async fn execute_in_transaction<F, T>(&self, f: F) -> Result<T, IdempotencyError>
    where
        F: FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, IdempotencyError>> + Send>> + Send,
        T: Send,
    {
        let _lock = self.transaction_lock.lock().await;
        
        let tx = self.pool.begin()
            .await
            .map_err(|e| IdempotencyError::TransactionFailed(e.to_string()))?;

        let result = f().await;

        match result {
            Ok(value) => {
                tx.commit().await.map_err(|e| IdempotencyError::TransactionFailed(e.to_string()))?;
                Ok(value)
            }
            Err(e) => {
                tx.rollback().await.map_err(|e| IdempotencyError::TransactionFailed(e.to_string()))?;
                Err(e)
            }
        }
    }
}
