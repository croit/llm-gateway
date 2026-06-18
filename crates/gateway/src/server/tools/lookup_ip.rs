// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `lookup_ip` — geolocate an *arbitrary* IP address or hostname using the
//! gateway's local IP2Location (GeoIP) database. This is the general lookup
//! the model reaches for when the user mentions some address ("where is
//! 2.59.44.8?", "what country does example.com resolve to?").
//!
//! It's the sibling of [`super::location::GetUserLocation`], which is
//! strictly about the *caller's own* position (browser GPS → the caller's
//! IP). This one takes a target and never touches the caller's IP, the
//! browser, or the request context — so it answers questions about third
//! parties, not "where am I".
//!
//! Hostnames are resolved to an address via the system resolver (DNS)
//! before lookup; raw IP literals skip that step. Private / reserved /
//! unmapped ranges geolocate to nothing and come back as `{known:false}`
//! with a reason, so the model can relay *why* rather than inventing a
//! place.

use std::net::IpAddr;

use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

pub struct LookupIp;

#[derive(serde::Deserialize)]
struct Args {
    target: String,
}

impl Tool for LookupIp {
    fn id(&self) -> &str {
        "lookup_ip"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Geolocate an IP address or hostname using the gateway's local \
             IP2Location (GeoIP) database — returns country, region, city, and \
             approximate coordinates. Accepts a raw IPv4/IPv6 address (e.g. \
             \"2.59.44.8\") or a hostname/domain (e.g. \"example.com\"), which is \
             resolved to an IP via DNS first. Use this for ANY IP or host the \
             user asks about, instead of guessing from training data. (For the \
             CURRENT user's own location, use get_user_location — this tool \
             never reads the caller's own IP.)",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "An IPv4/IPv6 address (e.g. \"2.59.44.8\") or a \
                                        hostname/domain (e.g. \"example.com\"). Hostnames are \
                                        resolved to an address via DNS before the lookup."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{target}}: {e}")))?;
            let target = args.target.trim().to_string();
            if target.is_empty() {
                return Err(ToolError::InvalidArgs("`target` must not be empty".into()));
            }

            let Some(geoip) = ctx.geoip.as_ref() else {
                return Ok(json!({
                    "known": false,
                    "note": "The GeoIP database isn't configured on this gateway, so \
                             IP/host lookups aren't available. Tell the user the feature \
                             is unavailable rather than guessing.",
                }));
            };

            // Raw IP literal → no DNS. Otherwise treat it as a hostname and
            // resolve to its address(es); `host` is set only on that path so
            // the result can echo back what was resolved.
            let (host, candidates): (Option<String>, Vec<IpAddr>) = match target.parse::<IpAddr>() {
                Ok(ip) => (None, vec![ip]),
                Err(_) => (Some(target.clone()), resolve_host(&target).await),
            };

            if candidates.is_empty() {
                return Ok(json!({
                    "known": false,
                    "host": host,
                    "note": format!(
                        "Couldn't resolve `{target}` — it isn't a valid IP address and DNS \
                         returned no records for it."
                    ),
                }));
            }

            // First resolved address the DB can place wins. (A host can
            // have several A/AAAA records; they normally share a location.)
            for ip in &candidates {
                if let Some(geo) = geoip.lookup(&ip.to_string()) {
                    return Ok(found_payload(&geo, *ip, host.as_deref()));
                }
            }

            // Resolved, but the DB has nothing — private / reserved / unmapped.
            Ok(json!({
                "known": false,
                "host": host,
                "ip": candidates[0].to_string(),
                "note": "Resolved to an address with no entry in the GeoIP database — \
                         likely a private, reserved, or otherwise unmapped range.",
            }))
        })
    }
}

/// Serialise a found `GeoLocation` and layer the tool-result envelope on
/// top (same shape as `get_user_location`'s IP branch). `host` carries the
/// original hostname when the target was resolved via DNS, so the model can
/// say "example.com (93.184.216.34) is in …".
fn found_payload(geo: &crate::server::geoip::GeoLocation, ip: IpAddr, host: Option<&str>) -> Value {
    let mut out = serde_json::to_value(geo).unwrap_or_else(|_| json!({}));
    if let Some(map) = out.as_object_mut() {
        map.insert("known".into(), json!(true));
        map.insert("source".into(), json!("geoip"));
        map.insert("precision".into(), json!("approximate"));
        map.insert("ip".into(), json!(ip.to_string()));
        if let Some(h) = host {
            map.insert("host".into(), json!(h));
        }
        map.insert(
            "note".into(),
            json!(
                "Approximate location from the IP2Location database (city-level at best). \
                 Coordinates, when present, are a coarse area centroid — not a precise position."
            ),
        );
    }
    out
}

/// Resolve a hostname to its IP addresses via the blocking system resolver
/// (`getaddrinfo`), kept off the async runtime with `spawn_blocking`. Port 0
/// is a placeholder — we only want the addresses. Any failure (NXDOMAIN,
/// no network, malformed name) collapses to an empty vec, which the caller
/// turns into a `{known:false}` "couldn't resolve" result.
async fn resolve_host(host: &str) -> Vec<IpAddr> {
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        (host.as_str(), 0u16)
            .to_socket_addrs()
            .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn ctx(geoip: Option<crate::server::geoip::GeoIp>) -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    #[test]
    fn schema_name_matches_id() {
        assert_eq!(LookupIp.id(), LookupIp.schema().function.name);
    }

    #[tokio::test]
    async fn missing_target_is_invalid_args() {
        let err = LookupIp.run(ctx(None).await, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn blank_target_is_invalid_args() {
        let err = LookupIp
            .run(ctx(None).await, json!({"target": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn no_geoip_configured_returns_known_false() {
        let out = LookupIp
            .run(ctx(None).await, json!({"target": "2.59.44.8"}))
            .await
            .unwrap();
        assert_eq!(out["known"], false);
        assert!(
            out["note"].as_str().unwrap().contains("isn't configured"),
            "{out:?}"
        );
    }

    // `localhost` resolves from /etc/hosts with no network, so this is a
    // deterministic, offline check that the DNS path actually returns
    // addresses (loopback) rather than an empty vec.
    #[tokio::test]
    async fn resolve_host_finds_localhost_loopback() {
        let ips = resolve_host("localhost").await;
        assert!(ips.iter().any(|ip| ip.is_loopback()), "got {ips:?}");
    }
}
