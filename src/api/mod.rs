pub mod health;
pub mod metrics;

use axum::{routing::get, Router};

use health::health_check;
use metrics::metrics_handler;

/// Create API router with all endpoints
#[allow(dead_code)]
pub fn create_router() -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_handler))
        .route("/ready", get(readiness_check))
}

/// Readiness check endpoint (checks dependencies)
#[allow(dead_code)]
async fn readiness_check() -> &'static str {
    // TODO: Check Redis, Docker, etc.
    "ready"
}
