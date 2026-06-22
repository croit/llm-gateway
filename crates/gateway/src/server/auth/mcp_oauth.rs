// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Generic MCP OAuth 2.1 client — the per-user authorization flow the gateway
//! drives to connect a user to a remote MCP server (Google, Atlassian, GitHub,
//! GitLab, …).
//!
//! Implements the MCP Authorization spec (rev. 2025-06-18) client side:
//! - **Discovery**: protected-resource metadata (RFC 9728) →
//!   authorization-server metadata (RFC 8414, with the OpenID-configuration
//!   path as a fallback) to learn the authorize / token / registration
//!   endpoints. Catalog config overrides win at every step.
//! - **Client identity**: Dynamic Client Registration (RFC 7591) when the AS
//!   advertises a registration endpoint and the connector opts in; otherwise a
//!   static `client_id` (+ optional secret) the admin configured.
//! - **Authorization code + PKCE** (S256), carrying the `resource` parameter
//!   (RFC 8707) so the issued token is audience-bound to the MCP server.
//! - **Token exchange + refresh**.
//!
//! This module is transport-agnostic and holds no state: callers persist the
//! PKCE verifier / DCR client between [`build_authorization`] and
//! [`exchange_code`] (see `db::user_mcp::pending_mcp_oauth`).

use std::time::Duration;

use jiff::Timestamp;
use rand::TryRngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum OauthError {
    #[error("OAuth discovery failed: {0}")]
    Discover(String),
    #[error("dynamic client registration failed: {0}")]
    Registration(String),
    #[error("token exchange failed: {0}")]
    Exchange(String),
    #[error("invalid URL `{0}`")]
    Url(String),
}

/// The OAuth endpoints a connector authorizes against.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
    /// RFC 7591 registration endpoint, when the AS supports DCR.
    pub registration_url: Option<String>,
}

/// Per-connector overrides from the catalog. Any set field wins over what
/// discovery returns; a fully-specified pair (authorize + token) skips
/// discovery entirely.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub registration_url: Option<String>,
}

/// Tokens returned by the authorization-server token endpoint.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<Timestamp>,
    /// Scopes the server actually granted (may differ from what we asked for).
    pub scopes: Vec<String>,
}

/// A freshly-minted PKCE pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// The data a caller needs to start an authorization: the URL to redirect the
/// browser to, plus the secrets to stash for the callback.
pub struct Authorization {
    pub url: String,
    pub state: String,
    pub pkce_verifier: String,
    pub endpoints: Endpoints,
    /// Set when this flow registered a client dynamically.
    pub dcr_client_id: Option<String>,
    pub dcr_client_secret: Option<String>,
}

// ---- discovery -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProtectedResourceMeta {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthServerMeta {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

/// A short-timeout, redirect-free HTTP client for discovery / token calls.
/// Redirect-free is an SSRF guard (operator-curated catalog, but defence in
/// depth) and surfaces misconfigured servers instead of hiding them.
pub fn discovery_http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .user_agent(concat!(
            "llm-gateway/",
            env!("CARGO_PKG_VERSION"),
            " mcp-oauth"
        ))
        .build()
        .unwrap_or_default()
}

/// Resolve the OAuth endpoints for an MCP server at `mcp_url`. Config overrides
/// win; otherwise walk RFC 9728 → RFC 8414 (OIDC config as fallback).
pub async fn discover(
    http: &reqwest::Client,
    mcp_url: &str,
    ov: &Overrides,
) -> Result<Endpoints, OauthError> {
    // Fully overridden → no network.
    if let (Some(a), Some(t)) = (ov.authorize_url.as_ref(), ov.token_url.as_ref()) {
        return Ok(Endpoints {
            authorize_url: a.clone(),
            token_url: t.clone(),
            registration_url: ov.registration_url.clone(),
        });
    }

    let base = Url::parse(mcp_url).map_err(|_| OauthError::Url(mcp_url.to_string()))?;

    // 1) Protected-resource metadata → authorization server URL. Try the
    //    server's `WWW-Authenticate` pointer first (RFC 9728 §5.1), then the
    //    well-known locations.
    let as_url = match fetch_protected_resource(http, &base, mcp_url).await {
        Some(servers) if !servers.is_empty() => servers.into_iter().next().unwrap(),
        _ => {
            // No protected-resource doc: assume the MCP server's own origin is
            // also the authorization server (common for all-in-one servers).
            origin(&base)
        }
    };

    // 2) Authorization-server metadata (RFC 8414), OIDC config as fallback.
    let meta = fetch_as_metadata(http, &as_url)
        .await
        .ok_or_else(|| OauthError::Discover(format!("no AS metadata at {as_url}")))?;

    let authorize_url = ov
        .authorize_url
        .clone()
        .or(meta.authorization_endpoint)
        .ok_or_else(|| OauthError::Discover("no authorization_endpoint".into()))?;
    let token_url = ov
        .token_url
        .clone()
        .or(meta.token_endpoint)
        .ok_or_else(|| OauthError::Discover("no token_endpoint".into()))?;
    let registration_url = ov.registration_url.clone().or(meta.registration_endpoint);

    Ok(Endpoints {
        authorize_url,
        token_url,
        registration_url,
    })
}

fn origin(u: &Url) -> String {
    let scheme = u.scheme();
    match u.host_str() {
        Some(host) => match u.port() {
            Some(p) => format!("{scheme}://{host}:{p}"),
            None => format!("{scheme}://{host}"),
        },
        None => u.as_str().trim_end_matches('/').to_string(),
    }
}

async fn fetch_protected_resource(
    http: &reqwest::Client,
    base: &Url,
    mcp_url: &str,
) -> Option<Vec<String>> {
    // Authoritative: hit the MCP endpoint unauthenticated and follow the
    // `resource_metadata` pointer in its `WWW-Authenticate` 401 (RFC 9728).
    if let Some(rm) = probe_resource_metadata_url(http, mcp_url).await
        && let Some(meta) = get_json::<ProtectedResourceMeta>(http, &rm).await
        && !meta.authorization_servers.is_empty()
    {
        return Some(meta.authorization_servers);
    }
    // Fallbacks: origin-rooted, RFC 9728 path-aware (well-known *before* the
    // resource path), then the path-suffixed variant some servers use.
    let path = base.path().trim_end_matches('/');
    let mut candidates = vec![format!(
        "{}/.well-known/oauth-protected-resource",
        origin(base)
    )];
    if !path.is_empty() {
        candidates.push(format!(
            "{}/.well-known/oauth-protected-resource{path}",
            origin(base)
        ));
    }
    candidates.push(format!(
        "{}/.well-known/oauth-protected-resource",
        base.as_str().trim_end_matches('/')
    ));
    for url in candidates {
        if let Some(meta) = get_json::<ProtectedResourceMeta>(http, &url).await
            && !meta.authorization_servers.is_empty()
        {
            return Some(meta.authorization_servers);
        }
    }
    None
}

/// Probe the MCP endpoint unauthenticated; return the `resource_metadata` URL
/// advertised in its `WWW-Authenticate` header, if any.
async fn probe_resource_metadata_url(http: &reqwest::Client, mcp_url: &str) -> Option<String> {
    validate_outbound_url(mcp_url).ok()?;
    let probe = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "llm-gateway", "version": "1"}}
    });
    let resp = http
        .post(mcp_url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&probe)
        .send()
        .await
        .ok()?;
    let header = resp.headers().get("www-authenticate")?.to_str().ok()?;
    parse_resource_metadata(header)
}

/// Extract the `resource_metadata="…"` value from a `WWW-Authenticate` header.
fn parse_resource_metadata(header: &str) -> Option<String> {
    let rest = &header[header.find("resource_metadata=")? + "resource_metadata=".len()..];
    let rest = rest.trim_start();
    if let Some(q) = rest.strip_prefix('"') {
        q.find('"').map(|end| q[..end].to_string())
    } else {
        let end = rest.find([',', ' ']).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

async fn fetch_as_metadata(http: &reqwest::Client, as_url: &str) -> Option<AuthServerMeta> {
    let trimmed = as_url.trim_end_matches('/');
    let mut candidates = vec![
        format!("{trimmed}/.well-known/oauth-authorization-server"),
        format!("{trimmed}/.well-known/openid-configuration"),
    ];
    // RFC 8414 path-aware: well-known inserted before the AS path
    // (e.g. AS `https://host/login/oauth` → `…/.well-known/oauth-authorization-server/login/oauth`).
    if let Ok(u) = Url::parse(as_url) {
        let path = u.path().trim_end_matches('/');
        if !path.is_empty() {
            candidates.push(format!(
                "{}/.well-known/oauth-authorization-server{path}",
                origin(&u)
            ));
            candidates.push(format!(
                "{}/.well-known/openid-configuration{path}",
                origin(&u)
            ));
        }
    }
    for url in candidates {
        if let Some(meta) = get_json::<AuthServerMeta>(http, &url).await
            && meta.authorization_endpoint.is_some()
            && meta.token_endpoint.is_some()
        {
            return Some(meta);
        }
    }
    None
}

async fn get_json<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &str) -> Option<T> {
    validate_outbound_url(url).ok()?;
    let resp = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<T>().await.ok()
}

/// SSRF guard for every URL the gateway itself fetches/POSTs during the OAuth
/// flow (discovery, AS metadata, registration, token). Requires `https`
/// (allowing `http` only for loopback so a self-hosted MCP server on localhost
/// still works in dev), and rejects literal link-local / unspecified /
/// multicast addresses — notably the cloud metadata endpoint
/// `169.254.169.254`. The catalog is admin-curated and private ranges
/// (10/8, 192.168/16, …) are intentionally allowed for internal deployments.
/// Residual: a hostname that *resolves* to a blocked IP (DNS-rebind) isn't
/// caught here — full protection would resolve-and-pin; this covers the
/// realistic literal-IP pivot.
pub fn validate_outbound_url(raw: &str) -> Result<(), OauthError> {
    let url = Url::parse(raw).map_err(|_| OauthError::Url(raw.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| OauthError::Url(format!("{raw}: no host")))?;
    let parsed_ip = host.parse::<std::net::IpAddr>().ok();
    let is_loopback = host == "localhost" || parsed_ip.map(|ip| ip.is_loopback()).unwrap_or(false);
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback) {
        return Err(OauthError::Url(format!(
            "{raw}: only https is allowed (http permitted only for localhost)"
        )));
    }
    if let Some(ip) = parsed_ip {
        let blocked = ip.is_unspecified()
            || ip.is_multicast()
            || match ip {
                std::net::IpAddr::V4(v4) => v4.is_link_local(), // 169.254/16, incl. metadata
                std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80, // fe80::/10
            };
        if blocked {
            return Err(OauthError::Url(format!("{raw}: blocked address range")));
        }
    }
    Ok(())
}

// ---- dynamic client registration (RFC 7591) -------------------------------

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// Register a public/confidential client at `registration_url`. Returns the
/// `(client_id, client_secret?)`.
pub async fn register_client(
    http: &reqwest::Client,
    registration_url: &str,
    redirect_uri: &str,
    client_name: &str,
    scopes: &[String],
) -> Result<(String, Option<String>), OauthError> {
    validate_outbound_url(registration_url)?;
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": scopes.join(" "),
    });
    let resp = http
        .post(registration_url)
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| OauthError::Registration(format!("POST {registration_url}: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OauthError::Registration(format!(
            "{registration_url} returned {status}: {}",
            snippet(&text)
        )));
    }
    let reg: RegistrationResponse = serde_json::from_str(&text)
        .map_err(|e| OauthError::Registration(format!("parsing registration JSON: {e}")))?;
    Ok((reg.client_id, reg.client_secret))
}

// ---- authorize + token -----------------------------------------------------

/// Generate a fresh PKCE pair (S256).
pub fn pkce() -> Pkce {
    let verifier = random_url_safe(64);
    let challenge = base64url_no_pad(&Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// A random URL-safe state token.
pub fn random_state() -> String {
    random_url_safe(32)
}

/// Build the authorization-redirect URL.
#[allow(clippy::too_many_arguments)]
pub fn build_authorize_url(
    authorize_url: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    pkce_challenge: &str,
    resource: &str,
) -> Result<String, OauthError> {
    let mut url =
        Url::parse(authorize_url).map_err(|_| OauthError::Url(authorize_url.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256")
            // RFC 8707: bind the issued token to the MCP server.
            .append_pair("resource", resource);
        if !scopes.is_empty() {
            q.append_pair("scope", &scopes.join(" "));
        }
        // Ask for a refresh token (Google requires these for offline access).
        q.append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
    }
    Ok(url.into())
}

#[derive(Debug, Deserialize)]
struct TokenResponseRaw {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

fn tokens_from_raw(raw: TokenResponseRaw) -> Tokens {
    let expires_at = raw
        .expires_in
        .map(|secs| Timestamp::now() + jiff::Span::new().seconds(secs));
    let scopes = raw
        .scope
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    Tokens {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_at,
        scopes,
    }
}

/// Exchange an authorization code for tokens.
#[allow(clippy::too_many_arguments)]
pub async fn exchange_code(
    http: &reqwest::Client,
    token_url: &str,
    code: &str,
    pkce_verifier: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: Option<&str>,
    resource: &str,
) -> Result<Tokens, OauthError> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", pkce_verifier),
        ("client_id", client_id),
        ("resource", resource),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    post_token(http, token_url, &form).await
}

/// Refresh an access token.
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<Tokens, OauthError> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    post_token(http, token_url, &form).await
}

async fn post_token(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<Tokens, OauthError> {
    validate_outbound_url(token_url)?;
    let resp = http
        .post(token_url)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|e| OauthError::Exchange(format!("POST {token_url}: {e}")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    // Surface a structured OAuth error first — some providers return it with a
    // 200 *and* an error body, so don't gate this on the status code (RFC 6749
    // §5.2 error shape: {"error": "...", "error_description": "..."}).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(err) = v.get("error").and_then(|e| e.as_str())
    {
        let desc = v
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let sep = if desc.is_empty() { "" } else { ": " };
        return Err(OauthError::Exchange(format!(
            "provider rejected the request — {err}{sep}{desc}"
        )));
    }
    if !status.is_success() {
        return Err(OauthError::Exchange(format!(
            "{token_url} returned {status}: {}",
            snippet(&text)
        )));
    }
    let raw: TokenResponseRaw = serde_json::from_str(&text).map_err(|_| {
        OauthError::Exchange(format!(
            "token endpoint returned no access_token. Response: {}",
            snippet(&text)
        ))
    })?;
    Ok(tokens_from_raw(raw))
}

// ---- small helpers (self-contained copies of the OIDC ones) ----------------

fn random_url_safe(byte_len: usize) -> String {
    let mut buf = vec![0u8; byte_len];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OsRng must produce bytes");
    base64url_no_pad(&buf)
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() >= 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn snippet(body: &str) -> String {
    const LIMIT: usize = 400;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = pkce();
        let expect = base64url_no_pad(&Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        assert!(!p.verifier.is_empty());
    }

    #[test]
    fn authorize_url_carries_pkce_resource_and_scopes() {
        let url = build_authorize_url(
            "https://accounts.example/auth",
            "client-123",
            "https://gw/integrations/callback",
            &["a".into(), "b".into()],
            "state-xyz",
            "challenge-abc",
            "https://mcp.example/",
        )
        .unwrap();
        assert!(url.starts_with("https://accounts.example/auth?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge=challenge-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("scope=a+b"));
        // resource is percent-encoded
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example%2F"));
    }

    #[test]
    fn authorize_url_omits_scope_when_empty() {
        let url = build_authorize_url(
            "https://a/auth",
            "c",
            "https://gw/cb",
            &[],
            "s",
            "ch",
            "https://mcp/",
        )
        .unwrap();
        assert!(!url.contains("scope="));
    }

    #[test]
    fn tokens_from_raw_computes_expiry_and_scopes() {
        let raw = TokenResponseRaw {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
            scope: Some("x y z".into()),
        };
        let t = tokens_from_raw(raw);
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token.as_deref(), Some("rt"));
        assert_eq!(t.scopes, vec!["x", "y", "z"]);
        assert!(t.expires_at.unwrap() > Timestamp::now());
    }

    #[test]
    fn discover_short_circuits_on_full_overrides() {
        // Both endpoints overridden → discover() must not need the network.
        let http = discovery_http();
        let ov = Overrides {
            authorize_url: Some("https://a/auth".into()),
            token_url: Some("https://a/token".into()),
            registration_url: Some("https://a/reg".into()),
        };
        let ep = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(discover(&http, "https://mcp.example/x", &ov))
            .unwrap();
        assert_eq!(ep.authorize_url, "https://a/auth");
        assert_eq!(ep.token_url, "https://a/token");
        assert_eq!(ep.registration_url.as_deref(), Some("https://a/reg"));
    }

    #[test]
    fn parses_resource_metadata_from_www_authenticate() {
        // GitHub's real header shape.
        let h = r#"Bearer error="invalid_request", error_description="No access token was provided in this request", resource_metadata="https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp/""#;
        assert_eq!(
            parse_resource_metadata(h).as_deref(),
            Some("https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp/")
        );
        // Unquoted value variant.
        let h2 = "Bearer resource_metadata=https://x/rm, realm=foo";
        assert_eq!(parse_resource_metadata(h2).as_deref(), Some("https://x/rm"));
        // Absent.
        assert_eq!(parse_resource_metadata("Bearer realm=foo"), None);
    }

    #[test]
    fn ssrf_guard_blocks_metadata_and_non_https() {
        // https public host: allowed.
        assert!(validate_outbound_url("https://accounts.google.com/o/oauth2/token").is_ok());
        // localhost over http: allowed (self-hosted dev).
        assert!(validate_outbound_url("http://localhost:9000/token").is_ok());
        assert!(validate_outbound_url("http://127.0.0.1:9000/token").is_ok());
        // non-loopback http: rejected.
        assert!(validate_outbound_url("http://example.com/token").is_err());
        // cloud metadata endpoint (v4 link-local): rejected even over https.
        assert!(validate_outbound_url("https://169.254.169.254/latest/meta-data").is_err());
        // unspecified + multicast: rejected.
        assert!(validate_outbound_url("https://0.0.0.0/x").is_err());
        // private range stays allowed (internal deployments).
        assert!(validate_outbound_url("https://10.1.2.3/token").is_ok());
        // garbage.
        assert!(validate_outbound_url("not a url").is_err());
    }

    #[test]
    fn origin_strips_path() {
        let u = Url::parse("https://mcp.example.com:8443/v1/sse").unwrap();
        assert_eq!(origin(&u), "https://mcp.example.com:8443");
    }
}
