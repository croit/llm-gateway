// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! CORS for the OpenAI-compatible `/v1/*` API surface.
//!
//! Browser-based apps (SPAs/PWAs) that call the gateway's `/v1` endpoints
//! directly need the server to (a) answer the CORS preflight
//! (`OPTIONS /v1/…`) *without* authentication and (b) echo the
//! `Access-Control-*` headers on every response — preflight, success,
//! error (4xx/5xx), and streaming (SSE) alike.
//!
//! This is a small purpose-built layer rather than rama's
//! [`rama::http::layer::cors`] for two reasons:
//!
//!   * **Scope.** Only the `/v1` API is cross-origin. The HTML UI, the
//!     OIDC `/auth/*` dance, and the session-cookie `/api/v0` surface are
//!     same-origin and are left untouched.
//!   * **Body type.** We only ever touch response *headers*, so the
//!     wrapped service's `Output` stays [`rama::http::Response`]
//!     (`Response<Body>`) — matching [`super::router::service`]'s
//!     signature and the test harness — instead of the
//!     `Response<OptionalBody<_>>` rama's `Cors` produces.
//!
//! Auth is a bearer token in the `Authorization` header, never a cookie,
//! so credentials mode is not needed: we reflect the request `Origin`
//! (falling back to `*` when none is sent, e.g. a non-browser client) and
//! deliberately do **not** emit `Access-Control-Allow-Credentials`.

use std::convert::Infallible;

use rama::http::{Body, HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use rama::{Layer, Service};

/// [`Layer`] that wraps a service with [`V1Cors`]. Apply it outside the
/// router's error handler so `RouterError`-rendered responses (404, 405,
/// …) on `/v1` paths are decorated too.
#[derive(Clone, Debug, Default)]
pub struct V1CorsLayer;

impl<S> Layer<S> for V1CorsLayer {
    type Service = V1Cors<S>;

    fn layer(&self, inner: S) -> Self::Service {
        V1Cors { inner }
    }

    fn into_layer(self, inner: S) -> Self::Service {
        V1Cors { inner }
    }
}

/// Adds CORS handling for `/v1/*`. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct V1Cors<S> {
    inner: S,
}

impl<S> Service<Request> for V1Cors<S>
where
    S: Service<Request, Output = Response, Error = Infallible>,
{
    type Output = Response;
    type Error = Infallible;

    async fn serve(&self, req: Request) -> Result<Self::Output, Self::Error> {
        // Only the `/v1` API surface is cross-origin. Everything else (the
        // HTML UI, `/auth/*`, the session `/api/v0`) is same-origin and
        // passes through untouched.
        let path = req.uri().path();
        if !(path == "/v1" || path.starts_with("/v1/")) {
            return self.inner.serve(req).await;
        }

        // Reflect the caller's `Origin` so any https origin is allowed;
        // fall back to `*` when none is sent. Safe to echo because we never
        // allow credentials (auth is a bearer token, not a cookie).
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("*"));

        // Preflight: answer directly and never invoke the inner service —
        // the browser sends `OPTIONS` without the `Authorization` header,
        // so the preflight must not be gated on auth.
        if req.method() == Method::OPTIONS {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            apply_cors_headers(resp.headers_mut(), origin);
            return Ok(resp);
        }

        // Actual request: run the handler, then decorate the response.
        // Header-only, so this works identically for JSON, error envelopes,
        // audio, and streaming (SSE) bodies.
        let mut resp = self.inner.serve(req).await?;
        apply_cors_headers(resp.headers_mut(), origin);
        Ok(resp)
    }
}

/// Write the four `Access-Control-*` headers — plus `Vary: Origin`, since
/// the allow-origin value is derived from the request — into `headers`.
fn apply_cors_headers(headers: &mut HeaderMap, origin: HeaderValue) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    // Reflected origin ⇒ shared caches must key on the `Origin` request
    // header. `append`, so any `Vary` an inner handler already set is kept.
    headers.append(header::VARY, HeaderValue::from_static("origin"));
}
