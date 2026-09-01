use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

use crate::key_pool::PoolError;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required configuration")]
    MissingRequired,
    #[error("invalid configuration")]
    Invalid,
    #[error("invalid upstream URL")]
    InvalidUrl,
    #[error("invalid bind address")]
    InvalidBindAddress,
    #[error("{0}")]
    KeyPool(#[from] PoolError),
}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyPool(PoolError::Empty { .. }) => "missing_key_pool",
            Self::KeyPool(PoolError::FileUnreadable { .. }) => "invalid_configuration",
            Self::MissingRequired | Self::Invalid | Self::InvalidUrl | Self::InvalidBindAddress => {
                "invalid_configuration"
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    success: bool,
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<&'static str>,
}

impl ErrorDetail {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            provider: None,
            failure_class: None,
        }
    }

    fn provider_failure(
        code: &'static str,
        message: &'static str,
        provider: &'static str,
        failure_class: &'static str,
    ) -> Self {
        Self {
            code,
            message,
            provider: Some(provider),
            failure_class: Some(failure_class),
        }
    }
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    InvalidInput,
    PayloadTooLarge,
    UpstreamCallerError(u16),
    UpstreamUnavailable,
    UpstreamExhausted {
        provider: &'static str,
        failure_class: &'static str,
    },
    UpstreamMalformed,
    GatewayTimeout,
    NoEligibleKey {
        provider: &'static str,
    },
    NotReady,
}

impl ApiError {
    fn status_and_detail(&self) -> (StatusCode, ErrorDetail) {
        match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorDetail::new("unauthorized", "authentication required"),
            ),
            Self::InvalidInput => (
                StatusCode::BAD_REQUEST,
                ErrorDetail::new("invalid_request", "request validation failed"),
            ),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorDetail::new("payload_too_large", "request is too large"),
            ),
            Self::UpstreamCallerError(status) if *status == 422 => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorDetail::new("upstream_rejected", "upstream rejected the request"),
            ),
            Self::UpstreamCallerError(_) => (
                StatusCode::BAD_REQUEST,
                ErrorDetail::new("upstream_rejected", "upstream rejected the request"),
            ),
            Self::UpstreamUnavailable => (
                StatusCode::BAD_GATEWAY,
                ErrorDetail::new("upstream_unavailable", "upstream provider unavailable"),
            ),
            Self::UpstreamExhausted {
                provider,
                failure_class,
            } => (
                StatusCode::BAD_GATEWAY,
                ErrorDetail::provider_failure(
                    "upstream_exhausted",
                    "upstream provider unavailable",
                    provider,
                    failure_class,
                ),
            ),
            Self::UpstreamMalformed => (
                StatusCode::BAD_GATEWAY,
                ErrorDetail::new(
                    "upstream_invalid_response",
                    "upstream returned an invalid response",
                ),
            ),
            Self::GatewayTimeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorDetail::new("upstream_timeout", "upstream request timed out"),
            ),
            Self::NoEligibleKey { provider } => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorDetail::provider_failure(
                    "provider_cooldown",
                    "provider keys temporarily unavailable",
                    provider,
                    "cooldown",
                ),
            ),
            Self::NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorDetail::new("not_ready", "required configuration is not ready"),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = self.status_and_detail();
        (
            status,
            Json(ErrorBody {
                success: false,
                error: detail,
            }),
        )
            .into_response()
    }
}
