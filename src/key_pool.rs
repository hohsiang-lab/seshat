use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Timeout,
    Connection,
    Status(u16),
}

impl FailureClass {
    pub fn from_status(status: u16) -> Self {
        Self::Status(status)
    }

    pub fn is_retryable(self) -> bool {
        match self {
            Self::Timeout | Self::Connection => true,
            Self::Status(status) => {
                matches!(status, 401 | 403 | 408 | 425 | 429 | 500..=599)
            }
        }
    }

    pub fn status_class(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connection => "connection",
            Self::Status(status) if status >= 500 => "5xx",
            Self::Status(401 | 403) => "auth",
            Self::Status(408 | 425 | 429) => "rate_or_timeout",
            Self::Status(_) => "http",
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PoolError {
    #[error("no usable keys configured for provider")]
    Empty { provider: String },
    #[error("unable to read provider key file")]
    FileUnreadable { provider: String },
}

#[derive(Clone)]
pub struct KeyCandidate {
    pub slot: usize,
    secret: String,
}

impl KeyCandidate {
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for KeyCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyCandidate")
            .field("slot", &self.slot)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

struct PoolState {
    cursor: usize,
    failure_streak: Vec<u32>,
    cooldown_until: Vec<Option<Instant>>,
}

struct PoolInner {
    provider: String,
    keys: Vec<String>,
    state: Mutex<PoolState>,
    initial_cooldown: Duration,
    max_cooldown: Duration,
}

#[derive(Clone)]
pub struct KeyPool {
    inner: Arc<PoolInner>,
}

impl fmt::Debug for KeyPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyPool")
            .field("provider", &self.inner.provider)
            .field("key_count", &self.inner.keys.len())
            .finish()
    }
}

impl KeyPool {
    pub fn from_keys<I, S>(keys: I, provider: impl Into<String>) -> Result<Self, PoolError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_policy(
            keys,
            provider,
            Duration::from_secs(30),
            Duration::from_secs(5 * 60),
        )
    }

    pub fn with_policy<I, S>(
        keys: I,
        provider: impl Into<String>,
        initial_cooldown: Duration,
        max_cooldown: Duration,
    ) -> Result<Self, PoolError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let provider = provider.into();
        let keys = normalize_keys(keys);
        if keys.is_empty() {
            return Err(PoolError::Empty { provider });
        }

        let len = keys.len();
        Ok(Self {
            inner: Arc::new(PoolInner {
                provider,
                keys,
                state: Mutex::new(PoolState {
                    cursor: 0,
                    failure_streak: vec![0; len],
                    cooldown_until: vec![None; len],
                }),
                initial_cooldown,
                max_cooldown,
            }),
        })
    }

    pub fn from_sources(
        file_contents: Option<&str>,
        environment_contents: Option<&str>,
        provider: impl Into<String>,
    ) -> Result<Self, PoolError> {
        let provider = provider.into();
        let selected = file_contents.or(environment_contents).unwrap_or_default();
        Self::from_keys(selected.lines().map(str::to_owned), provider)
    }

    pub fn from_file_or_env(
        file_path: Option<&str>,
        environment_contents: Option<&str>,
        provider: impl Into<String>,
    ) -> Result<Self, PoolError> {
        let provider = provider.into();
        let file_contents = file_path
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| {
                fs::read_to_string(path).map_err(|_| PoolError::FileUnreadable {
                    provider: provider.clone(),
                })
            })
            .transpose()?;
        Self::from_sources(file_contents.as_deref(), environment_contents, provider)
    }

    pub fn len(&self) -> usize {
        self.inner.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.keys.is_empty()
    }

    pub fn provider_name(&self) -> &str {
        &self.inner.provider
    }

    pub fn candidates(&self) -> Vec<KeyCandidate> {
        let now = Instant::now();
        let mut state = lock_state(&self.inner.state);
        let len = self.inner.keys.len();
        let start = state.cursor % len;
        let mut candidates = Vec::with_capacity(len);

        for offset in 0..len {
            let slot = (start + offset) % len;
            let available = state.cooldown_until[slot]
                .map(|until| until <= now)
                .unwrap_or(true);
            if available {
                candidates.push(KeyCandidate {
                    slot,
                    secret: self.inner.keys[slot].clone(),
                });
            }
        }

        if let Some(first) = candidates.first() {
            state.cursor = (first.slot + 1) % len;
        }
        candidates
    }

    pub fn mark_failure(&self, slot: usize, failure: FailureClass) {
        if !failure.is_retryable() || slot >= self.inner.keys.len() {
            return;
        }

        let mut state = lock_state(&self.inner.state);
        let streak = state.failure_streak[slot].saturating_add(1);
        state.failure_streak[slot] = streak;
        let multiplier = 1u32
            .checked_shl(streak.saturating_sub(1).min(31))
            .unwrap_or(u32::MAX);
        let cooldown = self
            .inner
            .initial_cooldown
            .checked_mul(multiplier)
            .unwrap_or(self.inner.max_cooldown)
            .min(self.inner.max_cooldown);
        state.cooldown_until[slot] = Some(Instant::now() + cooldown);
    }

    pub fn mark_success(&self, slot: usize) {
        if slot >= self.inner.keys.len() {
            return;
        }

        let mut state = lock_state(&self.inner.state);
        state.failure_streak[slot] = 0;
        state.cooldown_until[slot] = None;
    }
}

fn normalize_keys<I, S>(keys: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut seen = HashSet::new();
    keys.into_iter()
        .map(Into::into)
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

fn lock_state(state: &Mutex<PoolState>) -> std::sync::MutexGuard<'_, PoolState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, KeyPool};

    #[test]
    fn status_classes_are_sanitized() {
        assert_eq!(FailureClass::Status(503).status_class(), "5xx");
        assert_eq!(FailureClass::Status(401).status_class(), "auth");
        assert_eq!(FailureClass::Timeout.status_class(), "timeout");
    }

    #[test]
    fn successful_key_leaves_cooldown() {
        let pool = KeyPool::with_policy(
            ["a"],
            "firecrawl",
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(300),
        )
        .expect("pool should load");
        pool.mark_failure(0, FailureClass::Status(503));
        pool.mark_success(0);
        assert_eq!(pool.candidates().len(), 1);
    }
}
