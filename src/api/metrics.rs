use axum::{http::StatusCode, response::IntoResponse};
use prometheus::{Encoder, TextEncoder};

/// Prometheus metrics endpoint
/// GET /metrics
/// Returns Prometheus format metrics
#[allow(dead_code)]
pub async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to encode metrics: {}", e),
        )
            .into_response();
    }

    let metrics = String::from_utf8(buffer).unwrap_or_default();
    (StatusCode::OK, metrics).into_response()
}
