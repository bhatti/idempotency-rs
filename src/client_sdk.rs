use std::time::Duration;
use uuid::Uuid;
use backoff::{ExponentialBackoff, future::retry};

pub struct IdempotentClient {
    base_url: String,
    http_client: reqwest::Client,
    retry_config: RetryConfig,
}

#[derive(Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_interval: Duration,
    pub max_interval: Duration,
    pub multiplier: f64,
    pub enable_jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(10),
            multiplier: 2.0,
            enable_jitter: true,
        }
    }
}

impl IdempotentClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http_client: reqwest::Client::new(),
            retry_config: RetryConfig::default(),
        }
    }

    /// Make an idempotent API call with automatic retries
    pub async fn call<Req, Res>(&self,
        method: &str,
        path: &str,
        request: &Req,
    ) -> Result<Res, ClientError>
    where
        Req: Serialize,
        Res: DeserializeOwned,
    {
        // Generate idempotency key once for this logical operation
        let idempotency_key = Uuid::new_v4().to_string();

        // Configure exponential backoff with jitter
        let backoff = ExponentialBackoff {
            initial_interval: self.retry_config.initial_interval,
            max_interval: self.retry_config.max_interval,
            multiplier: self.retry_config.multiplier,
            randomization_factor: if self.retry_config.enable_jitter { 0.5 } else { 0.0 },
            max_elapsed_time: Some(Duration::from_secs(30)),
            ..Default::default()
        };

        // Retry with the same idempotency key
        retry(backoff, || async {
            let response = self.http_client
                .request(method.parse()?, &format!("{}{}", self.base_url, path))
                .header("Idempotency-Key", &idempotency_key)
                .json(request)
                .send()
                .await?;

            match response.status() {
                // Success - return result
                s if s.is_success() => {
                    Ok(response.json::<Res>().await?)
                }
                // Client error - don't retry (except 429)
                StatusCode::TOO_MANY_REQUESTS => {
                    let retry_after = response
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(1);

                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    Err(backoff::Error::transient(ClientError::RateLimited))
                }
                s if s.is_client_error() => {
                    // Don't retry client errors
                    Err(backoff::Error::permanent(ClientError::ClientError(s)))
                }
                // Server error - retry
                _ => {
                    Err(backoff::Error::transient(ClientError::ServerError))
                }
            }
        })
        .await
        .map_err(|e| match e {
            backoff::Error::Permanent(err) => err,
            backoff::Error::Transient { err, .. } => err,
        })
    }
}

// Circuit breaker integration
pub struct CircuitBreakerClient {
    inner: IdempotentClient,
    breaker: Arc<Mutex<CircuitBreaker>>,
}

impl CircuitBreakerClient {
    pub async fn call<Req, Res>(&self, /* params */) -> Result<Res, ClientError> {
        // Check circuit breaker state
        if self.breaker.lock().unwrap().is_open() {
            return Err(ClientError::CircuitOpen);
        }

        match self.inner.call(method, path, request).await {
            Ok(res) => {
                self.breaker.lock().unwrap().on_success();
                Ok(res)
            }
            Err(e) => {
                self.breaker.lock().unwrap().on_failure();
                Err(e)
            }
        }
    }
}
