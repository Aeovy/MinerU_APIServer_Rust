use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    ServiceUnavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::PayloadTooLarge(_) => "payload_too_large",
            Self::NotFound(_) => "not_found",
            Self::ServiceUnavailable(_) => "service_unavailable",
            Self::Internal(_) => "internal",
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn detail(&self) -> String {
        self.to_string()
    }

    pub fn internal_context(action: impl AsRef<str>, error: impl std::fmt::Display) -> Self {
        Self::Internal(format!("{}: {}", action.as_ref(), error))
    }

    pub fn bad_request_context(action: impl AsRef<str>, error: impl std::fmt::Display) -> Self {
        Self::BadRequest(format!("{}: {}", action.as_ref(), error))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let error_type = self.kind();
        let detail = self.detail();
        tracing::error!(status = %status, error_type, detail, "request failed");
        (
            status,
            Json(json!({ "detail": detail, "error_type": error_type })),
        )
            .into_response()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<axum::extract::multipart::MultipartError> for ApiError {
    fn from(error: axum::extract::multipart::MultipartError) -> Self {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            return Self::PayloadTooLarge(error.body_text());
        }
        Self::BadRequest(error.body_text())
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<async_openai::error::OpenAIError> for ApiError {
    fn from(error: async_openai::error::OpenAIError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use serde_json::Value;

    use super::ApiError;

    #[tokio::test]
    async fn api_error_response_includes_type_and_detail() {
        let response =
            ApiError::internal_context("Failed to write output file", "permission denied")
                .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let payload: Value = serde_json::from_slice(&bytes).expect("json payload");
        assert_eq!(payload["error_type"], "internal");
        assert_eq!(
            payload["detail"],
            "Failed to write output file: permission denied"
        );
    }
}
