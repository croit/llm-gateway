// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Read-only network/domain inspection tools: `dns_lookup`, `whois_lookup`,
//! and `tls_cert`. All three answer public, non-sensitive questions ("what
//! does this domain resolve to", "who registered it", "is this cert
//! expiring") — no secrets, no writes, no per-user state. They're the kind
//! of low-risk capability that's safe to leave always-on (see the tool
//! catalog's "Web & Network" group).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("llm-gateway/", env!("CARGO_PKG_VERSION"), " netcheck");

/// Shared HTTP client for the DoH / RDAP calls. Built per-invocation (these
/// tools fire infrequently); redirects are followed so RDAP bootstrap
/// (`rdap.org` → the authoritative registry server) works.
fn http_client() -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))
}

// ===========================================================================
// dns_lookup — DNS over HTTPS (Cloudflare's JSON API; no resolver dependency)
// ===========================================================================

pub struct DnsLookup;

#[derive(Deserialize)]
struct DnsArgs {
    name: String,
    #[serde(default)]
    record: Option<String>,
}

/// DoH numeric rr-type → label, for rendering the answer set.
fn rr_type_name(n: u64) -> String {
    match n {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        33 => "SRV",
        257 => "CAA",
        other => return other.to_string(),
    }
    .to_string()
}

const DNS_RECORD_TYPES: &[&str] = &[
    "A", "AAAA", "MX", "TXT", "NS", "CNAME", "SOA", "CAA", "SRV", "PTR",
];

impl Tool for DnsLookup {
    fn id(&self) -> &str {
        "dns_lookup"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Look up DNS records for a hostname (A, AAAA, MX, TXT, NS, CNAME, SOA, CAA, …) \
             via DNS-over-HTTPS. Returns the resolved records. Read-only public data — use \
             it to answer 'what does example.com resolve to', 'what are its mail servers', etc.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "The hostname to query, e.g. \"example.com\"." },
                    "record": {
                        "type": "string",
                        "enum": DNS_RECORD_TYPES,
                        "description": "DNS record type. Defaults to A."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: DnsArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{name, record?}}: {e}")))?;
            let name = args.name.trim();
            if name.is_empty() {
                return Err(ToolError::InvalidArgs("`name` must not be empty".into()));
            }
            let record = args
                .record
                .as_deref()
                .unwrap_or("A")
                .trim()
                .to_ascii_uppercase();
            if !DNS_RECORD_TYPES.contains(&record.as_str()) {
                return Err(ToolError::InvalidArgs(format!(
                    "unsupported record type `{record}` (one of {DNS_RECORD_TYPES:?})"
                )));
            }

            let resp = http_client()?
                .get("https://cloudflare-dns.com/dns-query")
                .query(&[("name", name), ("type", record.as_str())])
                .header("accept", "application/dns-json")
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("DoH request failed: {e}")))?;
            let body: Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Failed(format!("DoH response parse: {e}")))?;

            // RFC 8484/JSON: Status 0 = NOERROR, 3 = NXDOMAIN.
            let status = body.get("Status").and_then(Value::as_u64).unwrap_or(0);
            if status == 3 {
                return Ok(json!({
                    "name": name, "record_type": record, "found": false,
                    "note": "NXDOMAIN — the name does not exist.",
                }));
            }
            let answers: Vec<Value> = body
                .get("Answer")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|a| {
                            json!({
                                "type": rr_type_name(a.get("type").and_then(Value::as_u64).unwrap_or(0)),
                                "data": a.get("data").and_then(Value::as_str).unwrap_or(""),
                                "ttl": a.get("TTL").and_then(Value::as_u64),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "name": name,
                "record_type": record,
                "found": !answers.is_empty(),
                "answers": answers,
                "note": if answers.is_empty() { Some("No records of this type.") } else { None },
            }))
        })
    }
}

// ===========================================================================
// whois_lookup — domain registration via RDAP (the JSON successor to WHOIS)
// ===========================================================================

pub struct WhoisLookup;

#[derive(Deserialize)]
struct WhoisArgs {
    domain: String,
}

impl Tool for WhoisLookup {
    fn id(&self) -> &str {
        "whois_lookup"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Look up domain registration info (registrar, creation/expiry/updated dates, \
             status, nameservers) via RDAP — the modern JSON replacement for WHOIS. Read-only \
             public data. Use it for 'who registered example.com', 'when does it expire', etc. \
             Some TLDs don't publish RDAP; those return found:false.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["domain"],
                "properties": {
                    "domain": { "type": "string", "description": "The registered domain, e.g. \"example.com\"." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: WhoisArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{domain}}: {e}")))?;
            // Reduce a URL/host to a bare registrable-ish domain string.
            let domain = args
                .domain
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("")
                .trim_end_matches('.');
            if domain.is_empty() {
                return Err(ToolError::InvalidArgs("`domain` must not be empty".into()));
            }

            let resp = http_client()?
                .get(format!("https://rdap.org/domain/{domain}"))
                .header("accept", "application/rdap+json")
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("RDAP request failed: {e}")))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(json!({
                    "domain": domain, "found": false,
                    "note": "No RDAP record (domain unregistered, or its TLD has no RDAP service).",
                }));
            }
            if !resp.status().is_success() {
                return Err(ToolError::Failed(format!(
                    "RDAP server returned {}",
                    resp.status()
                )));
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Failed(format!("RDAP response parse: {e}")))?;
            Ok(rdap_summary(domain, &body))
        })
    }
}

/// Pull the human-relevant fields out of an RDAP domain object.
fn rdap_summary(domain: &str, body: &Value) -> Value {
    let event_date = |action: &str| -> Option<String> {
        body.get("events")?.as_array()?.iter().find_map(|e| {
            (e.get("eventAction").and_then(Value::as_str) == Some(action))
                .then(|| {
                    e.get("eventDate")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
        })
    };
    let status: Vec<String> = body
        .get("status")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let nameservers: Vec<String> = body
        .get("nameservers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|ns| {
                    ns.get("ldhName")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "domain": body.get("ldhName").and_then(Value::as_str).unwrap_or(domain),
        "found": true,
        "registrar": rdap_registrar(body),
        "registered": event_date("registration"),
        "expires": event_date("expiration"),
        "updated": event_date("last changed"),
        "status": status,
        "nameservers": nameservers,
        "note": "Approximate WHOIS-equivalent data from RDAP; fields vary by registry.",
    })
}

/// The registrar name lives in an entity whose `roles` include "registrar",
/// under its jCard (`vcardArray`) `fn` line: `["fn", {}, "text", "<name>"]`.
fn rdap_registrar(body: &Value) -> Option<String> {
    let entities = body.get("entities")?.as_array()?;
    let registrar = entities.iter().find(|e| {
        e.get("roles")
            .and_then(Value::as_array)
            .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some("registrar")))
    })?;
    let vcard = registrar
        .get("vcardArray")?
        .as_array()?
        .get(1)?
        .as_array()?;
    vcard.iter().find_map(|entry| {
        let e = entry.as_array()?;
        (e.first()?.as_str()? == "fn")
            .then(|| e.get(3)?.as_str().map(str::to_string))
            .flatten()
    })
}

// ===========================================================================
// tls_cert — inspect a server's presented TLS certificate
// ===========================================================================

pub struct TlsCert;

#[derive(Deserialize)]
struct TlsArgs {
    host: String,
    #[serde(default)]
    port: Option<u16>,
}

impl Tool for TlsCert {
    fn id(&self) -> &str {
        "tls_cert"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Inspect the TLS certificate a host presents: subject, issuer, validity dates, \
             days until expiry, and the certificate's DNS names (SAN). Connects and reads the \
             cert WITHOUT trusting it, so it also reports on expired or self-signed certs. \
             Read-only. Use it for 'is example.com's cert expiring', 'who issued it', etc.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["host"],
                "properties": {
                    "host": { "type": "string", "description": "Hostname to connect to, e.g. \"example.com\"." },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "TLS port. Defaults to 443." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: TlsArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{host, port?}}: {e}")))?;
            let host = args.host.trim().to_string();
            if host.is_empty() {
                return Err(ToolError::InvalidArgs("`host` must not be empty".into()));
            }
            let port = args.port.unwrap_or(443);

            match tokio::time::timeout(TIMEOUT, inspect_cert(&host, port)).await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(ToolError::Failed(e)),
                Err(_) => Err(ToolError::Failed(format!(
                    "timed out connecting to {host}:{port}"
                ))),
            }
        })
    }
}

async fn inspect_cert(host: &str, port: u16) -> Result<Value, String> {
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::ClientConfig;
    use tokio_rustls::rustls::pki_types::ServerName;
    use x509_parser::prelude::*;

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS config: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid SNI host `{host}`: {e}"))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| format!("TLS handshake: {e}"))?;

    let certs = tls
        .get_ref()
        .1
        .peer_certificates()
        .ok_or_else(|| "server presented no certificate".to_string())?;
    let leaf = certs
        .first()
        .ok_or_else(|| "empty certificate chain".to_string())?;
    let (_, cert) =
        parse_x509_certificate(leaf.as_ref()).map_err(|e| format!("parse certificate: {e}"))?;

    let not_before = cert.validity().not_before;
    let not_after = cert.validity().not_after;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days_until_expiry = (not_after.timestamp() - now_secs) / 86_400;
    let expired = now_secs > not_after.timestamp() || now_secs < not_before.timestamp();

    let dns_names: Vec<String> = match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|gn| match gn {
                GeneralName::DNSName(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    Ok(json!({
        "host": host,
        "port": port,
        "subject": cert.subject().to_string(),
        "issuer": cert.issuer().to_string(),
        "valid_from": not_before.to_string(),
        "valid_to": not_after.to_string(),
        "days_until_expiry": days_until_expiry,
        "expired": expired,
        "dns_names": dns_names,
        "chain_length": certs.len(),
    }))
}

/// A certificate verifier that accepts everything. We are *inspecting* the
/// presented cert, not establishing a trusted channel — so expired and
/// self-signed certs must NOT abort the handshake. Safe precisely because we
/// send no data over the connection; we read the cert and drop it.
#[derive(Debug)]
struct NoVerify;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        use tokio_rustls::rustls::SignatureScheme as S;
        vec![
            S::RSA_PKCS1_SHA256,
            S::RSA_PKCS1_SHA384,
            S::RSA_PKCS1_SHA512,
            S::ECDSA_NISTP256_SHA256,
            S::ECDSA_NISTP384_SHA384,
            S::ECDSA_NISTP521_SHA512,
            S::RSA_PSS_SHA256,
            S::RSA_PSS_SHA384,
            S::RSA_PSS_SHA512,
            S::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_ids() {
        assert_eq!(DnsLookup.id(), DnsLookup.schema().function.name);
        assert_eq!(WhoisLookup.id(), WhoisLookup.schema().function.name);
        assert_eq!(TlsCert.id(), TlsCert.schema().function.name);
    }

    #[test]
    fn rr_type_names_map_common_records() {
        assert_eq!(rr_type_name(1), "A");
        assert_eq!(rr_type_name(28), "AAAA");
        assert_eq!(rr_type_name(15), "MX");
        assert_eq!(rr_type_name(9999), "9999");
    }

    #[test]
    fn rdap_summary_extracts_registrar_and_events() {
        // Minimal RDAP-shaped object.
        let body = json!({
            "ldhName": "example.com",
            "status": ["client transfer prohibited"],
            "events": [
                {"eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z"},
                {"eventAction": "expiration", "eventDate": "2026-08-13T04:00:00Z"}
            ],
            "entities": [{
                "roles": ["registrar"],
                "vcardArray": ["vcard", [
                    ["version", {}, "text", "4.0"],
                    ["fn", {}, "text", "RESERVED-Internet Assigned Numbers Authority"]
                ]]
            }],
            "nameservers": [{"ldhName": "a.iana-servers.net"}]
        });
        let out = super::rdap_summary("example.com", &body);
        assert_eq!(out["found"], true);
        assert_eq!(out["registered"], "1995-08-14T04:00:00Z");
        assert_eq!(out["expires"], "2026-08-13T04:00:00Z");
        assert!(
            out["registrar"]
                .as_str()
                .unwrap()
                .contains("Numbers Authority")
        );
        assert_eq!(out["nameservers"][0], "a.iana-servers.net");
    }
}
