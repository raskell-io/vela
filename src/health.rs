use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HealthError {
    #[error("health check failed after {attempts} attempts: {last_error}")]
    Failed { attempts: u32, last_error: String },
    #[error("health check timed out after {0:?}")]
    Timeout(Duration),
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub url: String,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
}

impl HealthCheck {
    pub fn new(url: String) -> Self {
        Self {
            url,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            retries: 30,
        }
    }

    pub async fn wait_until_healthy(&self) -> Result<(), HealthError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| HealthError::Failed {
                attempts: 0,
                last_error: e.to_string(),
            })?;

        let mut last_error = String::new();

        for attempt in 1..=self.retries {
            match client.get(&self.url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(url = %self.url, attempt, "health check passed");
                    return Ok(());
                }
                Ok(resp) => {
                    last_error = format!("HTTP {}", resp.status());
                    tracing::debug!(url = %self.url, attempt, status = %resp.status(), "health check returned non-200");
                }
                Err(e) => {
                    last_error = e.to_string();
                    tracing::debug!(url = %self.url, attempt, err = %e, "health check failed");
                }
            }

            if attempt < self.retries {
                tokio::time::sleep(self.interval).await;
            }
        }

        Err(HealthError::Failed {
            attempts: self.retries,
            last_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_check_defaults() {
        let hc = HealthCheck::new("http://localhost:8080/health".into());
        assert_eq!(hc.retries, 30);
        assert_eq!(hc.interval, Duration::from_secs(1));
        assert_eq!(hc.timeout, Duration::from_secs(5));
    }
}
