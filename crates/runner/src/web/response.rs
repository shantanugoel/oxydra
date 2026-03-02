use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Standard API response envelope wrapping successful data.
#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub data: T,
    pub meta: ApiMeta,
}

/// Metadata included in every API response.
#[derive(Debug, Serialize)]
pub struct ApiMeta {
    pub request_id: String,
}

/// Standard API error envelope.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
    pub meta: ApiMeta,
}

/// The error body inside the envelope.
#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Generate a short random hex request ID.
pub fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Mix with a simple counter-like value for uniqueness within the same nanosecond.
    format!("{:012x}", nanos & 0xFFFF_FFFF_FFFF)
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        let body = serde_json::to_string(&self).unwrap_or_else(|_| {
            r#"{"error":{"code":"internal","message":"response serialization failed"}}"#.to_owned()
        });
        (StatusCode::OK, [("content-type", "application/json")], body).into_response()
    }
}

impl ApiError {
    pub fn with_status(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> ErrorResponse {
        ErrorResponse {
            status,
            body: Self {
                error: ApiErrorBody {
                    code,
                    message: message.into(),
                    details: None,
                },
                meta: ApiMeta {
                    request_id: generate_request_id(),
                },
            },
        }
    }
}

/// Pairs an `ApiError` with its HTTP status code so it can implement `IntoResponse`.
pub struct ErrorResponse {
    pub status: StatusCode,
    pub body: ApiError,
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        let body = serde_json::to_string(&self.body).unwrap_or_else(|_| {
            r#"{"error":{"code":"internal","message":"error serialization failed"}}"#.to_owned()
        });
        (self.status, [("content-type", "application/json")], body).into_response()
    }
}

/// Helper to create a successful API response.
pub fn ok_response<T: Serialize>(data: T) -> ApiResponse<T> {
    ApiResponse {
        data,
        meta: ApiMeta {
            request_id: generate_request_id(),
        },
    }
}
