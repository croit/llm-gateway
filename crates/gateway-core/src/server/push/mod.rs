// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Web Push: notify a user that an assistant turn they started finished while
//! they were away from the app.
//!
//! Two RFCs meet here:
//!   - **VAPID** (RFC 8292) authenticates the gateway to the push service. We
//!     hold one P-256 keypair; its public half is the browser's
//!     `applicationServerKey` at subscribe time, and every send carries a
//!     short-lived ES256 JWT signed with the private half. The keypair is
//!     generated once and persisted (private half sealed under the gateway's
//!     at-rest key, like every other stored secret) in `app_settings`.
//!   - **Message Encryption** (RFC 8291, see [`encrypt`]) encrypts the payload
//!     end-to-end for the subscription so the push service can't read it.
//!
//! [`PushSender`] owns the keypair + an HTTP client and does one thing:
//! [`PushSender::send`] a [`PushMessage`] to one subscription. The turn-finalize
//! hook in `spawn_assistant_worker` fans a message out over a user's
//! subscriptions and prunes any the service reports gone.

pub mod encrypt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use rand::TryRngCore;

use crate::server::crypto::Crypto;
use crate::server::db::push_subscriptions::PushSubscription;
use crate::server::db::{self, Pool};

/// `app_settings` key holding the sealed VAPID private scalar.
const VAPID_PRIVATE_KEY_SETTING: &str = "push.vapid.private";

/// VAPID JWTs live 12h — well inside the 24h ceiling push services enforce,
/// long enough that we don't re-sign on every message within a burst.
const JWT_TTL_SECONDS: i64 = 12 * 60 * 60;

/// How long the push service should retain an undelivered message (seconds).
/// A day: a turn-done ping is stale well before then, but this tolerates a
/// phone that's briefly offline.
const PUSH_TTL_SECONDS: u32 = 24 * 60 * 60;

/// The JSON delivered to the service worker's `push` handler. Kept small and
/// stable — `sw.js` reads exactly these fields.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PushMessage {
    /// Notification title, e.g. the conversation title.
    pub title: String,
    /// Body line under the title.
    pub body: String,
    /// Where `notificationclick` should navigate (a same-origin path).
    pub url: String,
    /// Coalescing tag so repeated pings for one conversation replace rather
    /// than stack — the session id.
    pub tag: String,
}

/// What happened when we posted to a push endpoint.
#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// The push service accepted the message (2xx).
    Delivered,
    /// The subscription is gone (404/410) — the caller should prune it.
    Gone,
    /// A transient or unexpected failure; left in place to retry next time.
    Failed,
}

/// A loaded VAPID keypair (RFC 8292).
struct Vapid {
    signing: SigningKey,
    /// The public key as base64url — the client's `applicationServerKey` and
    /// the `k=` parameter of the `Authorization` header.
    public_b64: String,
}

impl Vapid {
    fn from_private_bytes(priv32: &[u8]) -> anyhow::Result<Self> {
        let signing = SigningKey::from_slice(priv32)
            .map_err(|_| anyhow::anyhow!("VAPID private key is not a valid P-256 scalar"))?;
        let point = signing.verifying_key().to_encoded_point(false);
        let public_b64 = URL_SAFE_NO_PAD.encode(point.as_bytes());
        Ok(Self {
            signing,
            public_b64,
        })
    }

    /// A fresh keypair. Returns the raw private scalar (to seal + persist)
    /// alongside the loaded key.
    fn generate() -> anyhow::Result<([u8; 32], Self)> {
        for _ in 0..8 {
            let mut bytes = [0u8; 32];
            rand::rngs::OsRng
                .try_fill_bytes(&mut bytes)
                .map_err(|e| anyhow::anyhow!("RNG failure generating VAPID key: {e}"))?;
            if let Ok(v) = Self::from_private_bytes(&bytes) {
                return Ok((bytes, v));
            }
        }
        anyhow::bail!("could not generate a valid VAPID scalar")
    }

    /// The `vapid t=<jwt>, k=<pubkey>` Authorization header value for one
    /// endpoint. `aud` is the endpoint's origin; `now_secs` is the current
    /// Unix time (injected so the JWT-building logic is unit-testable).
    fn auth_header(&self, audience: &str, contact: &str, now_secs: i64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": audience,
            "exp": now_secs + JWT_TTL_SECONDS,
            "sub": contact,
        });
        let claims_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims).expect("serializing fixed-shape JWT claims never fails"),
        );
        let signing_input = format!("{header}.{claims_b64}");
        let sig: Signature = self.signing.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("vapid t={signing_input}.{sig_b64}, k={}", self.public_b64)
    }
}

/// Sends encrypted, VAPID-signed push messages. Built once at startup and
/// shared on `AppState`.
pub struct PushSender {
    vapid: Vapid,
    /// The VAPID `sub` claim — a `mailto:` or `https:` contact for the push
    /// service to reach the operator. From `[push].contact`.
    contact: String,
    http: reqwest::Client,
}

impl PushSender {
    /// Load (or first-time generate + persist) the VAPID keypair and build a
    /// sender. The keypair's public half is stable across restarts so browsers
    /// keep working without re-subscribing.
    ///
    /// The push HTTP client is dedicated (not the shared upstream one): a
    /// **10s timeout** so one stalled push service can't wedge a user's
    /// fan-out or leak the detached notify task, and **redirects disabled** so
    /// a push endpoint can't bounce the VAPID-signed POST to an internal host
    /// (defense-in-depth alongside the subscribe-time endpoint validation).
    pub async fn new(pool: &Pool, crypto: &Crypto, contact: String) -> anyhow::Result<Self> {
        let vapid = load_or_create_vapid(pool, crypto).await?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Ok(Self {
            vapid,
            contact,
            http,
        })
    }

    /// The base64url VAPID public key — served to the client as its
    /// `applicationServerKey`.
    pub fn public_key(&self) -> &str {
        &self.vapid.public_b64
    }

    /// Encrypt `message` for `sub` and POST it to the push service.
    pub async fn send(&self, sub: &PushSubscription, message: &PushMessage) -> SendOutcome {
        let payload = match serde_json::to_vec(message) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(error = %err, "serializing push message");
                return SendOutcome::Failed;
            }
        };
        self.send_bytes(sub, &payload).await
    }

    async fn send_bytes(&self, sub: &PushSubscription, payload: &[u8]) -> SendOutcome {
        let ua_public = match b64url_decode(&sub.p256dh) {
            Some(k) => k,
            None => {
                tracing::warn!(endpoint = %sub.endpoint, "subscription p256dh is not base64url");
                return SendOutcome::Failed;
            }
        };
        let auth = match b64url_decode(&sub.auth) {
            Some(a) => a,
            None => {
                tracing::warn!(endpoint = %sub.endpoint, "subscription auth is not base64url");
                return SendOutcome::Failed;
            }
        };
        let body = match encrypt::encrypt(&ua_public, &auth, payload) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(error = %err, endpoint = %sub.endpoint, "encrypting push payload");
                return SendOutcome::Failed;
            }
        };

        let Some(audience) = endpoint_origin(&sub.endpoint) else {
            tracing::warn!(endpoint = %sub.endpoint, "push endpoint has no parseable origin");
            return SendOutcome::Failed;
        };
        let now = jiff::Timestamp::now().as_second();
        let auth_header = self.vapid.auth_header(&audience, &self.contact, now);

        let resp = self
            .http
            .post(&sub.endpoint)
            .header(reqwest::header::AUTHORIZATION, auth_header)
            .header(reqwest::header::CONTENT_ENCODING, "aes128gcm")
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header("TTL", PUSH_TTL_SECONDS.to_string())
            .header("Urgency", "normal")
            .body(body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => SendOutcome::Delivered,
            Ok(r) if matches!(r.status().as_u16(), 404 | 410) => SendOutcome::Gone,
            Ok(r) => {
                tracing::warn!(status = %r.status(), endpoint = %sub.endpoint, "push service rejected message");
                SendOutcome::Failed
            }
            Err(err) => {
                tracing::warn!(error = %err, endpoint = %sub.endpoint, "posting to push service");
                SendOutcome::Failed
            }
        }
    }
}

/// Load the persisted VAPID keypair, generating + storing one on first use (or
/// if the stored value can't be decrypted, e.g. the at-rest key rotated).
async fn load_or_create_vapid(pool: &Pool, crypto: &Crypto) -> anyhow::Result<Vapid> {
    if let Some(stored) = db::app_settings::get(pool, VAPID_PRIVATE_KEY_SETTING).await?
        && let Some(vapid) = open_sealed(&stored, crypto)
    {
        return Ok(vapid);
    }
    if db::app_settings::get(pool, VAPID_PRIVATE_KEY_SETTING)
        .await?
        .is_some()
    {
        tracing::warn!(
            "stored VAPID key could not be decrypted (at-rest key changed?); regenerating — \
             existing push subscriptions will stop delivering until re-subscribed"
        );
    }
    let (priv_bytes, vapid) = Vapid::generate()?;
    let sealed = crypto.seal(&priv_bytes)?;
    let stored = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&sealed.nonce),
        URL_SAFE_NO_PAD.encode(&sealed.ciphertext),
    );
    db::app_settings::set(pool, VAPID_PRIVATE_KEY_SETTING, &stored).await?;
    tracing::info!("generated a new VAPID keypair for Web Push");
    Ok(vapid)
}

/// Parse the `nonce.ciphertext` stored form, decrypt, and load the key.
/// `None` on any malformation so the caller regenerates rather than failing.
fn open_sealed(stored: &str, crypto: &Crypto) -> Option<Vapid> {
    let (nonce_b64, ct_b64) = stored.split_once('.')?;
    let nonce = b64url_decode(nonce_b64)?;
    let ct = b64url_decode(ct_b64)?;
    let priv_bytes = crypto.open(&nonce, &ct).ok()?;
    Vapid::from_private_bytes(&priv_bytes).ok()
}

/// The origin (`scheme://host[:port]`) of a push endpoint — the VAPID `aud`.
fn endpoint_origin(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    let origin = url.origin();
    if origin.is_tuple() {
        Some(origin.ascii_serialization())
    } else {
        None
    }
}

/// Decode base64url, tolerating optional padding (browsers omit it).
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Validate a subscription the client is trying to register, BEFORE it's
/// stored — the gateway later POSTs (VAPID-signed) to `endpoint` whenever the
/// owner's turn finishes, so an unchecked endpoint is a blind-SSRF vector.
///
/// Requires:
/// - `endpoint` is an `https` URL whose host is a public name/address — not
///   loopback, private, link-local, or unspecified (blocks the cloud metadata
///   IP, `localhost`, and RFC 1918 / ULA targets), and
/// - `p256dh` decodes to a 65-byte uncompressed P-256 point and `auth` to a
///   16-byte secret (so we never persist junk that can only ever fail to
///   encrypt).
///
/// This can't stop a public hostname that resolves to an internal IP (DNS
/// rebinding), but combined with redirects-disabled on the push client it
/// closes the practical vectors.
pub fn validate_subscription(endpoint: &str, p256dh: &str, auth: &str) -> Result<(), String> {
    let url = url::Url::parse(endpoint).map_err(|_| "endpoint is not a valid URL".to_string())?;
    if url.scheme() != "https" {
        return Err("push endpoint must be https".to_string());
    }
    match url.host() {
        Some(url::Host::Domain(d)) if !d.eq_ignore_ascii_case("localhost") => {}
        Some(url::Host::Ipv4(ip))
            if !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()) => {}
        Some(url::Host::Ipv6(ip)) if !is_disallowed_ipv6(&ip) => {}
        _ => return Err("push endpoint host is not allowed".to_string()),
    }
    match b64url_decode(p256dh) {
        Some(k) if k.len() == 65 && k[0] == 0x04 => {}
        _ => return Err("keys.p256dh must be a base64url 65-byte P-256 point".to_string()),
    }
    match b64url_decode(auth) {
        Some(a) if a.len() == 16 => {}
        _ => return Err("keys.auth must be a base64url 16-byte secret".to_string()),
    }
    Ok(())
}

/// Loopback / unspecified / unique-local (`fc00::/7`) / link-local (`fe80::/10`)
/// IPv6 — the addresses a push endpoint must not target. Hand-rolled because
/// `Ipv6Addr::is_unique_local` / `is_unicast_link_local` are still unstable.
fn is_disallowed_ipv6(ip: &std::net::Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let first = ip.segments()[0];
    (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vapid() -> Vapid {
        Vapid::from_private_bytes(&[5u8; 32]).unwrap()
    }

    #[test]
    fn public_key_is_a_65_byte_uncompressed_point() {
        let bytes = URL_SAFE_NO_PAD.decode(vapid().public_b64).unwrap();
        assert_eq!(bytes.len(), 65);
        assert_eq!(bytes[0], 0x04, "uncompressed SEC1 point prefix");
    }

    #[test]
    fn auth_header_is_a_verifiable_es256_jwt() {
        use p256::ecdsa::signature::Verifier;

        let v = vapid();
        let header = v.auth_header(
            "https://fcm.googleapis.com",
            "mailto:ops@example.com",
            1_700_000_000,
        );
        // Shape: "vapid t=<jwt>, k=<pubkey>".
        let rest = header.strip_prefix("vapid t=").expect("vapid scheme");
        let (jwt, k) = rest.split_once(", k=").expect("t= and k= params");
        assert_eq!(k, v.public_b64, "k= carries our public key");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");

        // Header + claims decode and carry the expected fields.
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        assert_eq!(claims["exp"], 1_700_000_000 + JWT_TTL_SECONDS);

        // The signature verifies against the VAPID public key over
        // "header.claims" — i.e. it's a real ES256 JWT.
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        v.signing
            .verifying_key()
            .verify(signing_input.as_bytes(), &sig)
            .expect("VAPID JWT signature verifies");
    }

    #[test]
    fn endpoint_origin_strips_the_path() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/abc123").as_deref(),
            Some("https://fcm.googleapis.com"),
        );
        assert_eq!(
            endpoint_origin("https://updates.push.services.mozilla.com:443/wpush/v2/xyz")
                .as_deref(),
            Some("https://updates.push.services.mozilla.com"),
        );
        assert_eq!(endpoint_origin("not a url"), None);
    }

    #[test]
    fn b64url_decode_tolerates_padding() {
        assert_eq!(b64url_decode("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(b64url_decode("YWJj=="), Some(b"abc".to_vec()));
    }

    // Valid RFC 8291 §5 key material for the validation tests.
    const P256DH: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

    #[test]
    fn validate_subscription_accepts_a_public_https_endpoint() {
        assert!(
            validate_subscription("https://fcm.googleapis.com/fcm/send/abc", P256DH, AUTH).is_ok()
        );
    }

    #[test]
    fn validate_subscription_rejects_ssrf_and_non_https_targets() {
        // Cloud metadata IP, loopback, private range, link-local, localhost, http.
        for bad in [
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://localhost/x",
            "http://fcm.googleapis.com/x",
            "ftp://fcm.googleapis.com/x",
            "not a url",
        ] {
            assert!(
                validate_subscription(bad, P256DH, AUTH).is_err(),
                "should reject endpoint: {bad}"
            );
        }
    }

    #[test]
    fn validate_subscription_rejects_bad_key_material() {
        // Wrong p256dh length / prefix, wrong auth length.
        assert!(validate_subscription("https://fcm.googleapis.com/x", "AAAA", AUTH).is_err());
        assert!(validate_subscription("https://fcm.googleapis.com/x", P256DH, "AAAA").is_err());
        assert!(
            validate_subscription("https://fcm.googleapis.com/x", "not!base64!", AUTH).is_err()
        );
    }
}
