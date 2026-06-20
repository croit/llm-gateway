// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! HTTP surface of the runner, built on the same rama stack as the
//! gateway. Two routes:
//!   - `GET  /healthz` — liveness/readiness for the Quadlet + gateway.
//!   - `POST /run`     — execute one [`RunRequest`], return a [`RunResponse`].
//!
//! There is no auth here by design: the runner must be reachable **only**
//! from the gateway over an internal network (and, when remote, fronted by
//! mTLS). It executes arbitrary code — exposing it publicly is RCE-as-a-
//! service. The deployment (Quadlet network + firewall) is the boundary.

use std::sync::Arc;

use rama::http::layer::error_handling::ErrorHandlerLayer;
use rama::http::server::HttpServer;
use rama::http::service::web::Router;
use rama::http::service::web::extract::State;
use rama::http::service::web::response::{IntoResponse, Json};
use rama::http::{Request, Response, StatusCode, header};
use rama::layer::{ArcLayer, Layer};
use rama::net::address::SocketAddress;
use serde_json::json;
use shared::sandbox::{RunError, RunRequest};

use crate::pool::{Pool, RunnerError};

/// Shared handler state. The pool already carries its own `Arc<Config>`,
/// so the handlers only need the pool.
pub struct RunnerState {
    pub pool: Arc<Pool>,
}

pub fn router(state: Arc<RunnerState>) -> Router<Arc<RunnerState>> {
    Router::new_with_state(state)
        .with_get("/healthz", async || Json(json!({"status": "ok"})))
        .with_post("/run", run)
}

/// POST /run — decode the request, execute it, return the result.
async fn run(State(state): State<Arc<RunnerState>>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };
    let request: RunRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("body is not a RunRequest: {e}"),
            );
        }
    };

    match state.pool.run(&request).await {
        Ok(resp) => json_ok(&resp),
        Err(RunnerError::Busy) => err(StatusCode::SERVICE_UNAVAILABLE, "sandbox at capacity"),
        Err(RunnerError::NetworkUnavailable) => err(
            StatusCode::BAD_REQUEST,
            "network egress requested but not configured on this runner",
        ),
        Err(RunnerError::Backend(e)) => {
            tracing::warn!(error = %e, "sandbox backend failed");
            err(StatusCode::BAD_GATEWAY, &format!("sandbox backend: {e}"))
        }
    }
}

async fn read_body(body: rama::http::Body) -> Result<rama::bytes::Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading request body: {e}"))
}

fn json_ok<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(s) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            s,
        )
            .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("serialising response: {e}"),
        ),
    }
}

/// Error responses use the shared [`RunError`] envelope so the gateway
/// gets one predictable shape on every non-2xx.
fn err(status: StatusCode, message: &str) -> Response {
    let body = serde_json::to_string(&RunError {
        error: message.to_string(),
    })
    .unwrap_or_else(|_| "{\"error\":\"serialisation failed\"}".to_string());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Build the servable service (router + the layers that make rama's
/// `Router` clone-able and infallible), mirroring the gateway's `service`.
fn service(
    state: Arc<RunnerState>,
) -> impl rama::Service<Request, Output = Response, Error = std::convert::Infallible> + Clone {
    let router = router(state);
    (ArcLayer::new(), ErrorHandlerLayer::default()).into_layer(router)
}

pub async fn serve(state: Arc<RunnerState>, addr: SocketAddress) -> anyhow::Result<()> {
    HttpServer::default()
        .listen(addr, service(state))
        .await
        .map_err(|e| anyhow::anyhow!("rama listen: {e}"))?;
    Ok(())
}
