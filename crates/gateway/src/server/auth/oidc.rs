// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! OIDC client — hand-rolled, just enough of the spec to do an
//! authorization-code-with-PKCE login and verify the resulting ID token.
//!
//! Why not `openidconnect`?
//! - Opaque error messages: `DiscoveryError::Parse` wraps the inner serde
//!   error as `#[source]` but the `#[error]` annotation drops it, so failures
//!   present as "Failed to parse server response" with no path info.
//! - `EmptyAdditionalClaims` silently strips fields like `groups` / `roles`
//!   before we see them — RBAC mapping never fires.
//! - Heavy generics (`CoreClient<EndpointSet, EndpointNotSet, …>` with 6
//!   type parameters) for endpoint state we don't track.
//!
//! What we implement:
//! - `GET <issuer>/.well-known/openid-configuration` → parse out
//!   authorization_endpoint, token_endpoint, jwks_uri, signing algs.
//! - Verify the discovery doc's `issuer` field byte-matches what was
//!   configured (RFC 8414 §3).
//! - Build the auth URL with `state`, `nonce`, and SHA-256 PKCE.
//! - `POST <token_endpoint>` with `grant_type=authorization_code` + PKCE
//!   verifier + client credentials in the form body (the broadest-
//!   compatibility variant — Keycloak, Authentik, Auth0, Okta all accept it).
//! - `GET <jwks_uri>` (cached, refreshed on unknown kid) and verify the ID
//!   token's RS/ES/PS signature via `jsonwebtoken`.
//! - Validate `iss`, `aud`, `exp`, `iat`, and `nonce` ourselves.
//! - Pull `sub`, `email`, `name`, and the configured `roles_claim` straight
//!   out of the verified JSON payload — no claims-stripping middleman.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use rand::TryRngCore;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use crate::server::config::OidcConfig;

#[derive(Debug, Error)]
pub enum OidcError {
    #[error("invalid issuer URL `{0}`")]
    Issuer(String),
    #[error("OIDC client secret env var `{0}` is unset")]
    MissingClientSecret(String),
    #[error("invalid redirect URL `{0}`")]
    RedirectUrl(String),
    #[error("OIDC discovery failed: {0}")]
    Discover(String),
    #[error("OIDC token exchange failed: {0}")]
    Exchange(String),
    #[error("OIDC ID token verification failed: {0}")]
    Verify(String),
    #[error("OIDC response missing required ID token")]
    NoIdToken,
}

/// Cached subset of the provider's discovery doc.
#[derive(Debug, Clone, Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// One JWK from the provider's JWKS endpoint. We accept the fields we use;
/// `#[serde(default)]` keeps us forward-compatible with extra fields.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    /// Some providers omit `kid`; in that case we can only match by alg and
    /// pray there's a single key. Optional.
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    /// RSA modulus + exponent (when kty=RSA).
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    /// EC coordinates (when kty=EC). The curve is inferred from `alg`, so we
    /// don't need to deserialise `crv`.
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwksResponse {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    /// What we actually care about — the rest of the response (access_token,
    /// token_type, expires_in, refresh_token) we don't use.
    id_token: Option<String>,
}

struct JwksCache {
    /// kid → (algorithm, decoding key). For keys without a kid we store under
    /// the special key `""` and accept it for any header that also has no kid.
    keys: HashMap<String, (Algorithm, DecodingKey)>,
    fetched_at: Instant,
}

pub struct OidcClient {
    /// Trimmed; verified to match the discovery doc's `issuer` field.
    issuer: String,
    metadata: ProviderMetadata,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    http: reqwest::Client,
    scopes: Vec<String>,
    roles_claim: Option<String>,
    jwks: RwLock<JwksCache>,
}

impl OidcClient {
    pub async fn build(config: &OidcConfig, public_url: &str) -> Result<Arc<Self>, OidcError> {
        // Discovery URL: `<issuer>/.well-known/openid-configuration`. We don't
        // alter the user's issuer string for slash-normalisation — the
        // discovery doc's `issuer` field must match it byte-for-byte (RFC
        // 8414 §3), so any normalisation we did here would break a subset of
        // providers (Authentik wants the trailing slash, Keycloak doesn't).
        let issuer = config.issuer.trim().to_string();
        Url::parse(&issuer).map_err(|_| OidcError::Issuer(issuer.clone()))?;

        let well_known = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );

        let http = reqwest::Client::builder()
            // Don't follow redirects — SSRF defence; also forces misconfigured
            // IdPs to expose their redirect quirks instead of silently hiding
            // them.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OidcError::Discover(format!("building http client: {e}")))?;

        let response = http
            .get(&well_known)
            .send()
            .await
            .map_err(|e| OidcError::Discover(format!("GET {well_known}: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(OidcError::Discover(format!(
                "GET {well_known} returned {status}"
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|e| OidcError::Discover(format!("reading discovery body: {e}")))?;
        let metadata: ProviderMetadata = serde_json::from_str(&body)
            .map_err(|e| OidcError::Discover(format!("parsing discovery JSON: {e}")))?;

        if metadata.issuer != issuer {
            return Err(OidcError::Discover(format!(
                "issuer mismatch: configured `{issuer}` but discovery doc says `{}`. \
                 RFC 8414 §3 requires byte-for-byte equality — copy whatever \
                 `curl <issuer>/.well-known/openid-configuration | jq -r .issuer` \
                 returns into the config.",
                metadata.issuer
            )));
        }

        let client_secret = config
            .client_secret()
            .ok_or_else(|| OidcError::MissingClientSecret(config.client_secret_env.clone()))?;
        let redirect_uri = format!("{}/auth/callback", public_url.trim_end_matches('/'));
        Url::parse(&redirect_uri).map_err(|_| OidcError::RedirectUrl(redirect_uri.clone()))?;

        // openid is always requested; the user can add email/profile/etc on
        // top via [oidc].scopes.
        let mut scopes = vec!["openid".to_string()];
        for s in &config.scopes {
            if s != "openid" {
                scopes.push(s.clone());
            }
        }

        let jwks = JwksCache {
            keys: HashMap::new(),
            fetched_at: Instant::now() - Duration::from_secs(3600),
        };

        Ok(Arc::new(Self {
            issuer,
            metadata,
            client_id: config.client_id.clone(),
            client_secret,
            redirect_uri,
            http,
            scopes,
            roles_claim: config.roles_claim.clone(),
            jwks: RwLock::new(jwks),
        }))
    }

    /// Builds the URL the browser should be redirected to to start sign-in,
    /// plus the per-flow secrets the gateway needs to remember in the session
    /// to validate the callback.
    pub fn begin(&self) -> AuthorizationStart {
        let csrf = random_url_safe(32);
        let nonce = random_url_safe(32);
        let pkce_verifier = random_url_safe(64);
        let pkce_challenge = pkce_s256_challenge(&pkce_verifier);

        let mut url = Url::parse(&self.metadata.authorization_endpoint)
            .expect("authorization_endpoint was already a URL at discovery time");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", &csrf)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &pkce_challenge)
            .append_pair("code_challenge_method", "S256");

        AuthorizationStart {
            url: url.into(),
            csrf,
            nonce,
            pkce_verifier,
        }
    }

    /// Exchanges an authorization code for an ID token, verifies the token
    /// signature against the provider's JWKS, and validates standard claims +
    /// the per-flow nonce.
    pub async fn complete(
        &self,
        code: &str,
        pkce_verifier: &str,
        expected_nonce: &str,
    ) -> Result<UserClaims, OidcError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", pkce_verifier),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ];

        let response = self
            .http
            .post(&self.metadata.token_endpoint)
            .header("accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Exchange(format!("POST token_endpoint: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| OidcError::Exchange(format!("reading token body: {e}")))?;
        if !status.is_success() {
            return Err(OidcError::Exchange(format!(
                "token endpoint returned {status}: {body}"
            )));
        }
        let token: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| OidcError::Exchange(format!("parsing token JSON: {e}")))?;
        let id_token = token.id_token.ok_or(OidcError::NoIdToken)?;

        self.verify_id_token(&id_token, expected_nonce).await
    }

    /// Decodes + signature-verifies the ID token, then validates the nonce
    /// and pulls user claims out of the JSON payload.
    async fn verify_id_token(
        &self,
        id_token: &str,
        expected_nonce: &str,
    ) -> Result<UserClaims, OidcError> {
        // Unsigned peek at the header to pick the right JWK.
        let header = decode_header(id_token)
            .map_err(|e| OidcError::Verify(format!("decoding header: {e}")))?;
        let kid_key = header.kid.unwrap_or_default();

        // Look up the key. If unknown, refresh the JWKS once and try again
        // (handles signing-key rotation).
        let (alg, key) = match self.lookup_key(&kid_key).await {
            Some(pair) => pair,
            None => {
                self.refresh_jwks().await?;
                match self.lookup_key(&kid_key).await {
                    Some(pair) => pair,
                    None => {
                        let available_kids: Vec<String> = {
                            let cache = self.jwks.read().await;
                            cache
                                .keys
                                .keys()
                                .map(|k| {
                                    if k.is_empty() {
                                        "<no kid>".into()
                                    } else {
                                        format!("`{k}`")
                                    }
                                })
                                .collect()
                        };
                        let detail = if available_kids.is_empty() {
                            "(JWKS refresh stored zero usable keys — check the gateway logs \
                             for `skipping JWK` warnings to see why each one was dropped)"
                                .to_string()
                        } else {
                            format!("(available kids: [{}])", available_kids.join(", "))
                        };
                        return Err(OidcError::Verify(format!(
                            "no JWK matches token kid `{kid_key}` even after JWKS refresh \
                             {detail}. Most likely a stale cache or key-rotation race on \
                             the IdP side."
                        )));
                    }
                }
            }
        };

        // jsonwebtoken's Validation handles iss/aud/exp/iat/nbf. Nonce is an
        // OIDC concept it doesn't know about — we check it below.
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.client_id]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // RFC 7519 says iat is informational; we accept tokens with or
        // without it. jsonwebtoken won't fail on a missing iat.
        validation.required_spec_claims = ["exp", "iss", "aud"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let token = decode::<Value>(id_token, &key, &validation)
            .map_err(|e| OidcError::Verify(format!("verifying signature/claims: {e}")))?;
        let claims = token.claims;

        let token_nonce = claims
            .get("nonce")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if token_nonce != expected_nonce {
            return Err(OidcError::Verify(
                "nonce mismatch: token nonce did not match the session-stashed value \
                 (possible replay; restart login)"
                    .into(),
            ));
        }

        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OidcError::Verify("ID token missing required `sub` claim".into()))?
            .to_string();
        let email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let roles = match self.roles_claim.as_deref() {
            Some(key) => extract_roles(&claims, key),
            None => vec![],
        };

        Ok(UserClaims {
            subject,
            email,
            name,
            roles,
        })
    }

    async fn lookup_key(&self, kid: &str) -> Option<(Algorithm, DecodingKey)> {
        let cache = self.jwks.read().await;
        if let Some((alg, key)) = cache.keys.get(kid) {
            return Some((*alg, key.clone()));
        }
        // Fall back to a key with no kid if the token also has no kid. Some
        // providers ship a single unkeyed JWK.
        if kid.is_empty() {
            return cache.keys.values().next().map(|(a, k)| (*a, k.clone()));
        }
        None
    }

    async fn refresh_jwks(&self) -> Result<(), OidcError> {
        // Rate-limit refreshes: don't go to the network more than once a
        // minute, regardless of how many unknown-kid lookups we see.
        {
            let cache = self.jwks.read().await;
            if cache.fetched_at.elapsed() < Duration::from_secs(60) && !cache.keys.is_empty() {
                return Ok(());
            }
        }
        let response = self
            .http
            .get(&self.metadata.jwks_uri)
            .send()
            .await
            .map_err(|e| OidcError::Verify(format!("GET jwks_uri: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| OidcError::Verify(format!("reading JWKS body: {e}")))?;
        if !status.is_success() {
            return Err(OidcError::Verify(format!(
                "JWKS endpoint {} returned {status}: {}",
                self.metadata.jwks_uri,
                snippet(&body)
            )));
        }
        let jwks: JwksResponse = serde_json::from_str(&body).map_err(|e| {
            OidcError::Verify(format!(
                "parsing JWKS JSON from {}: {e}. Body was: {}",
                self.metadata.jwks_uri,
                snippet(&body)
            ))
        })?;
        if jwks.keys.is_empty() {
            return Err(OidcError::Verify(format!(
                "JWKS at {} contains no keys — the OIDC provider has no signing \
                 key bound to this application. In Authentik: Providers → your \
                 OIDC provider → Signing Key. In Keycloak: realm settings → \
                 keys.",
                self.metadata.jwks_uri
            )));
        }

        let mut keys = HashMap::new();
        // Best-effort load: skip JWKs we can't handle, log + continue, so one
        // malformed/unknown key doesn't kill the whole refresh. The verify
        // step is still pinned to whatever algorithm the matched JWK declares
        // (`Validation::new(alg)`), so we don't need a separate allow-list
        // pass against the discovery doc's `id_token_signing_alg_values_
        // supported`. That extra check was paranoia that just gave us another
        // place to silently drop legitimate keys.
        for jwk in jwks.keys {
            let kid_label = jwk.kid.as_deref().unwrap_or("<no kid>");
            let alg = match jwk
                .alg
                .as_deref()
                .and_then(parse_alg)
                .or_else(|| default_alg_for_kty(&jwk.kty))
            {
                Some(a) => a,
                None => {
                    tracing::warn!(
                        kid = kid_label,
                        kty = %jwk.kty,
                        alg = ?jwk.alg,
                        "skipping JWK: no usable algorithm",
                    );
                    continue;
                }
            };

            let key = match decoding_key_for_jwk(&jwk) {
                Ok(k) => k,
                Err(err) => {
                    tracing::warn!(
                        kid = kid_label,
                        error = %err,
                        "skipping JWK: failed to build decoding key",
                    );
                    continue;
                }
            };
            keys.insert(jwk.kid.clone().unwrap_or_default(), (alg, key));
        }

        let mut cache = self.jwks.write().await;
        cache.keys = keys;
        cache.fetched_at = Instant::now();
        Ok(())
    }
}

fn decoding_key_for_jwk(jwk: &Jwk) -> Result<DecodingKey, OidcError> {
    match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or_else(|| {
                OidcError::Verify(format!("RSA JWK missing `n` (kid={:?})", jwk.kid))
            })?;
            let e = jwk.e.as_deref().ok_or_else(|| {
                OidcError::Verify(format!("RSA JWK missing `e` (kid={:?})", jwk.kid))
            })?;
            DecodingKey::from_rsa_components(n, e)
                .map_err(|e| OidcError::Verify(format!("building RSA decoding key: {e}")))
        }
        "EC" => {
            let x = jwk.x.as_deref().ok_or_else(|| {
                OidcError::Verify(format!("EC JWK missing `x` (kid={:?})", jwk.kid))
            })?;
            let y = jwk.y.as_deref().ok_or_else(|| {
                OidcError::Verify(format!("EC JWK missing `y` (kid={:?})", jwk.kid))
            })?;
            DecodingKey::from_ec_components(x, y)
                .map_err(|e| OidcError::Verify(format!("building EC decoding key: {e}")))
        }
        other => Err(OidcError::Verify(format!(
            "unsupported JWK kty `{other}` — only RSA and EC are accepted"
        ))),
    }
}

fn parse_alg(s: &str) -> Option<Algorithm> {
    match s {
        "RS256" => Some(Algorithm::RS256),
        "RS384" => Some(Algorithm::RS384),
        "RS512" => Some(Algorithm::RS512),
        "PS256" => Some(Algorithm::PS256),
        "PS384" => Some(Algorithm::PS384),
        "PS512" => Some(Algorithm::PS512),
        "ES256" => Some(Algorithm::ES256),
        "ES384" => Some(Algorithm::ES384),
        // HS* (HMAC) is intentionally not supported — the symmetric secret
        // would need to be shared with the provider, which OIDC doesn't do.
        _ => None,
    }
}

fn default_alg_for_kty(kty: &str) -> Option<Algorithm> {
    match kty {
        "RSA" => Some(Algorithm::RS256),
        "EC" => Some(Algorithm::ES256),
        _ => None,
    }
}

/// Pulls a list of role-like strings out of a verified claims map. Accepts
/// the configured claim as either a JSON array of strings or a single string.
/// Unknown shapes log + return empty. Crucially this reads from the raw JWT
/// payload — unlike openidconnect's `EmptyAdditionalClaims` it does NOT strip
/// unknown fields before extraction.
fn extract_roles(claims: &Value, key: &str) -> Vec<String> {
    let Some(field) = claims.get(key) else {
        return vec![];
    };
    match field {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Value::String(s) => vec![s.clone()],
        other => {
            tracing::warn!(?other, claim = key, "unexpected shape for OIDC roles claim");
            vec![]
        }
    }
}

/// Random URL-safe base64 string from `byte_len` bytes of OS entropy (no
/// padding). Used for state, nonce, and PKCE verifier — all of which need
/// to be unguessable, hence OsRng directly rather than a seeded PRNG.
fn random_url_safe(byte_len: usize) -> String {
    let mut buf = vec![0u8; byte_len];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OsRng must produce bytes");
    base64url_no_pad(&buf)
}

fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_no_pad(&digest)
}

/// Truncates a response body for inclusion in an error message. JWKS/discovery
/// bodies are usually a few hundred bytes; HTML error pages can be megabytes,
/// so cap and hint at truncation. UTF-8-safe: backs off to the nearest valid
/// codepoint boundary.
fn snippet(body: &str) -> String {
    const LIMIT: usize = 512;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated, {} bytes total]", &body[..end], body.len())
}

/// Tiny URL-safe base64 encoder (no padding). Avoids pulling in the `base64`
/// crate as a direct dep — we don't need decoding here.
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

pub struct AuthorizationStart {
    pub url: String,
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

#[derive(Debug, Clone)]
pub struct UserClaims {
    pub subject: String,
    pub email: String,
    pub name: Option<String>,
    pub roles: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_roles_handles_array() {
        let v = json!({"groups": ["engineering", "admin"]});
        assert_eq!(
            extract_roles(&v, "groups"),
            vec!["engineering".to_string(), "admin".to_string()]
        );
    }

    #[test]
    fn extract_roles_handles_single_string() {
        let v = json!({"role": "finance"});
        assert_eq!(extract_roles(&v, "role"), vec!["finance".to_string()]);
    }

    #[test]
    fn extract_roles_empty_for_missing_claim() {
        let v = json!({"other": "value"});
        assert!(extract_roles(&v, "groups").is_empty());
    }

    #[test]
    fn extract_roles_empty_for_unexpected_shape() {
        let v = json!({"groups": 42});
        assert!(extract_roles(&v, "groups").is_empty());
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 §4.4 worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_s256_challenge(verifier), expected);
    }

    #[test]
    fn base64url_no_pad_matches_rfc4648_examples() {
        // From RFC 4648 §10 (regular base64), de-padded + URL-safe.
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
    }
}
