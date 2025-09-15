-- PostgreSQL migration for idempotency tables
-- This can be used instead of SQLite for production deployments

CREATE TYPE idempotency_status AS ENUM ('pending', 'completed', 'failed_retryable', 'failed_permanent');

CREATE TABLE IF NOT EXISTS idempotency_records (
    key UUID NOT NULL,
    user_id TEXT NOT NULL,
    request_path TEXT NOT NULL,
    request_method TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    status idempotency_status NOT NULL DEFAULT 'pending',
    response_status_code INTEGER,
    response_headers JSONB,
    response_body BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    locked_until TIMESTAMPTZ,

    PRIMARY KEY (key, user_id),

    -- Indexes for performance
    INDEX idx_expires_at ON idempotency_records(expires_at),
    INDEX idx_user_id ON idempotency_records(user_id),
    INDEX idx_status ON idempotency_records(status),
    INDEX idx_locked_until ON idempotency_records(locked_until)
);

-- Function to automatically clean up expired records
CREATE OR REPLACE FUNCTION cleanup_expired_idempotency_records()
RETURNS INTEGER AS $$
DECLARE
    deleted_count INTEGER;
BEGIN
    DELETE FROM idempotency_records
    WHERE expires_at < NOW();

    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    RETURN deleted_count;
END;
$$ LANGUAGE plpgsql;

-- Schedule cleanup job (requires pg_cron extension)
-- CREATE EXTENSION IF NOT EXISTS pg_cron;
-- SELECT cron.schedule('cleanup-idempotency', '0 */1 * * *', 'SELECT cleanup_expired_idempotency_records()');
