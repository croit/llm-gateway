// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Best-effort client-IP → location via IP2Location LITE DB11.
//! This is what lets the assistant
//! answer "what's the weather here?" without the user spelling out a
//! city: the `get_user_location` tool resolves the caller's IP to a
//! coarse city/country (and a precise browser position when the user
//! shared one — see `db::users`).
//!
//! **Optional by design.** With no `[geoip]` config and no database
//! file the gateway boots and serves exactly as before; every lookup
//! returns `None` and the tool falls back to the browser position (or
//! reports the location is unknown). There is no `panic`/`expect` on a
//! missing file, a bad file, or a missing token anywhere in here.
//!
//! **Hot-reloadable.** The reader is memory-mapped and held behind an
//! `RwLock<Option<Arc<DB>>>` so a new file can be swapped in without a
//! restart — whether the optional weekly updater wrote it or an
//! operator dropped one in. [`GeoIp::watch`] runs a filesystem watcher
//! that calls [`GeoIp::reload`] on change. Lookups clone the `Arc` out
//! under a short read lock and run the (synchronous, ~microsecond)
//! `ip_lookup` *without* holding the lock, so a reload only ever blocks
//! a reader for the pointer swap.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ip2location::{DB, Record};
use rama::http::HeaderMap;
use serde::Serialize;

pub mod update;

/// One resolved location. Every field is optional: a DB11 row may carry
/// only a country, and a sparse / reserved IP may carry nothing at all.
/// Serialises straight into the tool result the model sees.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct GeoLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// ISO 3166 two-letter code, e.g. `"DE"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
}

impl GeoLocation {
    /// True when the lookup found *something* worth returning. IP2Location
    /// stores `"-"`/`"0"` and `0.0/0.0` coordinates for unmapped or
    /// reserved ranges; once those are scrubbed (see [`clean`]) such a
    /// row is all-`None` and we treat it as a miss.
    fn is_useful(&self) -> bool {
        self.country_code.is_some()
            || self.city.is_some()
            || self.region.is_some()
            || (self.latitude.is_some() && self.longitude.is_some())
    }
}

/// Hot-swappable handle to the IP2Location reader. Cheap to clone (an
/// `Arc` inside); shared via `AppState` and threaded onto `ToolContext`.
#[derive(Clone)]
pub struct GeoIp {
    db_path: PathBuf,
    reader: Arc<RwLock<Option<Arc<DB>>>>,
}

impl GeoIp {
    /// Build a handle for `db_path` and attempt an initial load. A
    /// missing or invalid file is **not** an error — `reader` stays
    /// `None` and lookups return `None` until a valid file appears (the
    /// watcher loads it then). Call [`GeoIp::watch`] afterwards to enable
    /// hot-reload.
    pub fn new(db_path: PathBuf) -> Self {
        let me = Self {
            db_path,
            reader: Arc::new(RwLock::new(None)),
        };
        me.reload();
        me
    }

    /// The database path this handle watches / loads from.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// True once a usable database is loaded in memory.
    pub fn is_loaded(&self) -> bool {
        self.reader
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// (Re)load the BIN from disk and atomically swap it in. On failure
    /// (file gone, truncated, mid-rewrite) it logs a warning and keeps
    /// whatever reader was active — never panics, never leaves a torn
    /// state. Cheap + idempotent; the watcher calls it on every relevant
    /// file event.
    pub fn reload(&self) {
        match DB::from_file(&self.db_path) {
            Ok(db @ DB::LocationDb(_)) => {
                *self.reader.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(db));
                tracing::info!(path = %self.db_path.display(), "geoip database loaded");
            }
            Ok(DB::ProxyDb(_)) => {
                tracing::warn!(
                    path = %self.db_path.display(),
                    "geoip path points at an IP2Proxy database, not an IP2Location one — \
                     ignoring; client-IP location lookups stay disabled"
                );
            }
            Err(err) => {
                // Expected when the feature is simply unused, or briefly
                // while the updater rewrites the file. Either way we keep
                // serving with whatever (possibly none) reader we had.
                tracing::warn!(
                    error = %err, path = %self.db_path.display(),
                    "geoip database not available — client-IP location lookups disabled"
                );
            }
        }
    }

    /// Resolve a client IP string to a location. Returns `None` when no
    /// database is loaded, the string isn't a routable IP, the IP isn't
    /// in the DB, or the row carries nothing useful. Synchronous and
    /// fast (microseconds against the mmap); safe to call from async
    /// code without `spawn_blocking`.
    pub fn lookup(&self, ip: &str) -> Option<GeoLocation> {
        let ip: IpAddr = ip.trim().parse().ok()?;
        // Loopback / private / unspecified addresses never geolocate; skip
        // the DB hit (and the misleading 0,0 some rows carry for them).
        if is_non_routable(&ip) {
            return None;
        }
        // Clone the Arc out under a short read lock, then release it so a
        // concurrent reload only contends for the pointer swap.
        let db = {
            let guard = self.reader.read().unwrap_or_else(|p| p.into_inner());
            guard.clone()?
        };
        let Record::LocationDb(rec) = db.ip_lookup(ip).ok()? else {
            return None;
        };
        // IP2Location stores 0.0/0.0 for ranges it can't place. The cast
        // widens the DB's f32 to the f64 we expose.
        let (latitude, longitude) = match (rec.latitude, rec.longitude) {
            (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => {
                (Some(lat as f64), Some(lon as f64))
            }
            _ => (None, None),
        };
        let loc = GeoLocation {
            latitude,
            longitude,
            country_code: rec.country.as_ref().and_then(|c| clean(&c.short_name)),
            country: rec.country.as_ref().and_then(|c| clean(&c.long_name)),
            region: rec.region.as_deref().and_then(clean),
            city: rec.city.as_deref().and_then(clean),
        };
        loc.is_useful().then_some(loc)
    }

    /// Spawn a filesystem watcher that hot-reloads the database whenever
    /// the file changes — an operator drop-in, or the weekly updater's
    /// atomic rename. Watches the *parent directory* (not the file) so a
    /// replace-by-rename, or the file first appearing at runtime, is
    /// still seen. Best-effort: if the directory can't be watched we log
    /// and carry on without hot-reload (lookups still work against
    /// whatever loaded at startup).
    ///
    /// Runs on a dedicated OS thread because `notify`'s recommended
    /// watcher delivers events on a std channel; the work it does
    /// (`reload`) is synchronous, so no tokio runtime is involved.
    pub fn watch(&self) {
        use notify::{EventKind, RecursiveMode, Watcher};

        let dir = watch_dir(&self.db_path);
        // Make sure the directory exists so we can both watch it and let
        // the updater write into it. If this fails we bail out of *only*
        // the watcher — startup still succeeds and lookups still work
        // against whatever loaded; a file that appears later just won't be
        // auto-reloaded until the next restart.
        if !dir.exists()
            && let Err(err) = std::fs::create_dir_all(&dir)
        {
            tracing::warn!(error = %err, dir = %dir.display(), "could not create geoip dir; hot-reload disabled");
            return;
        }

        let this = self.clone();
        let watched_file = self.db_path.clone();
        std::thread::Builder::new()
            .name("geoip-watch".into())
            .spawn(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                let mut watcher = match notify::recommended_watcher(tx) {
                    Ok(w) => w,
                    Err(err) => {
                        tracing::warn!(error = %err, "geoip watcher init failed; hot-reload disabled");
                        return;
                    }
                };
                if let Err(err) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    tracing::warn!(error = %err, dir = %dir.display(), "geoip watch failed; hot-reload disabled");
                    return;
                }
                tracing::debug!(dir = %dir.display(), "geoip file watcher active");
                // The `watcher` must stay alive for the duration of this
                // loop — it stops watching when dropped.
                for event in rx {
                    let Ok(event) = event else { continue };
                    if !matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        continue;
                    }
                    // Only react to events touching our file. A rename
                    // surfaces as a path on the new name; `ends_with`
                    // tolerates relative vs canonical path differences.
                    let hit = event.paths.iter().any(|p| {
                        p == &watched_file
                            || watched_file
                                .file_name()
                                .is_some_and(|name| p.file_name() == Some(name))
                    });
                    if hit {
                        tracing::debug!("geoip file changed; reloading");
                        this.reload();
                    }
                }
            })
            .map(|_| ())
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "could not spawn geoip watch thread; hot-reload disabled");
            });
    }
}

/// IP2Location sentinels for "no data". A field carrying one of these
/// (or empty) is reported as absent rather than literal `"-"`.
fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t == "-" || t == "?" || t.eq_ignore_ascii_case("n/a") {
        None
    } else {
        Some(t.to_string())
    }
}

/// The directory to watch for changes to `db_path`. `Path::parent()`
/// returns `Some("")` for a bare filename (a relative `db_path` in the
/// cwd) and `None` only for the filesystem root — both mean "the
/// directory the file lives in", i.e. `.`. Watching an empty path makes
/// `notify` fail with "No path was found", which silently disabled
/// hot-reload before this normalised them.
fn watch_dir(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Addresses that can never geolocate — skip the DB and avoid returning
/// a bogus "0,0 / -" for them. Conservative: only the cases `std`
/// classifies on stable Rust.
fn is_non_routable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Whether the browser is on a secure context — the precondition for
/// `navigator.geolocation`. Browsers allow it only over HTTPS or from a
/// `localhost`/loopback origin; on a plain-HTTP LAN origin (e.g.
/// `http://192.168.1.5:8080`) it's blocked outright, so there's no point
/// prompting. Inferred from the request:
///   - `X-Forwarded-Proto: https` — a TLS-terminating reverse proxy;
///   - a `localhost` / `127.0.0.1` / `[::1]` `Host` — loopback dev;
///   - else the operator's `public_url` being HTTPS (covers a proxy that
///     terminates TLS but doesn't set `X-Forwarded-Proto`).
pub fn transport_is_secure(headers: &HeaderMap, public_url: &str) -> bool {
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        && proto.trim().eq_ignore_ascii_case("https")
    {
        return true;
    }
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        let host = host.trim();
        if host == "::1"
            || host.starts_with("localhost")
            || host.starts_with("127.0.0.1")
            || host.starts_with("[::1]")
        {
            return true;
        }
    }
    public_url.starts_with("https://")
}

/// Pull the caller's source IP from proxy headers: the left-most
/// `X-Forwarded-For` entry (the original client) wins, then `X-Real-IP`;
/// `CF-Connecting-IP` is honoured first for Cloudflare-fronted setups.
/// Header-only — pair with [`peer_ip`] for the direct-socket fallback
/// (`client_ip(headers).or_else(|| peer_ip(req))`). Trusts the front-most
/// proxy not to spoof these (the standard assumption behind a trusted
/// load balancer); best-effort, not a security control.
pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    fn valid(s: &str) -> Option<String> {
        let s = s.trim();
        s.parse::<IpAddr>().ok().map(|_| s.to_string())
    }
    if let Some(ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(valid)
    {
        return Some(ip);
    }
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|list| list.split(',').next())
        .and_then(valid)
    {
        return Some(ip);
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(valid)
}

/// The directly-connected client's IP, read from the TCP socket — rama
/// stashes it in the request extensions as `SocketInfo`. This is the
/// fallback when no proxy header is present (a gateway with no load
/// balancer in front): it yields the real peer for remote clients, or
/// `127.0.0.1` for a localhost connection. So between [`client_ip`] and
/// this, the source IP is *always* known.
pub fn peer_ip(req: &rama::http::Request) -> Option<String> {
    // `extensions()` is rama's `ExtensionsRef` trait method; `SocketInfo`
    // is what the TCP listener stashes there (see rama's own
    // `set_forwarded` layer, which reads it the same way).
    use rama::extensions::ExtensionsRef;
    req.extensions()
        .get_ref::<rama::net::stream::SocketInfo>()
        .map(|s| s.peer_addr().ip_addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_scrubs_sentinels() {
        assert_eq!(clean("Berlin"), Some("Berlin".into()));
        assert_eq!(clean(" - "), None);
        assert_eq!(clean(""), None);
        assert_eq!(clean("?"), None);
        assert_eq!(clean("N/A"), None);
    }

    #[test]
    fn watch_dir_handles_bare_and_nested_paths() {
        // Bare filename (relative db_path in cwd) → watch ".", not "".
        assert_eq!(
            watch_dir(Path::new("IP2LOCATION-LITE-DB11.BIN")),
            PathBuf::from(".")
        );
        // Nested path → watch its real parent directory.
        assert_eq!(
            watch_dir(Path::new("data/ip2location/db.BIN")),
            PathBuf::from("data/ip2location")
        );
        assert_eq!(
            watch_dir(Path::new("/etc/gateway/db.BIN")),
            PathBuf::from("/etc/gateway")
        );
    }

    #[test]
    fn non_routable_skips() {
        assert!(is_non_routable(&"127.0.0.1".parse().unwrap()));
        assert!(is_non_routable(&"10.0.0.5".parse().unwrap()));
        assert!(is_non_routable(&"192.168.1.1".parse().unwrap()));
        assert!(is_non_routable(&"::1".parse().unwrap()));
        assert!(!is_non_routable(&"8.8.8.8".parse().unwrap()));
        assert!(!is_non_routable(
            &"2a00:1450:4001:80e::200e".parse().unwrap()
        ));
    }

    #[test]
    fn lookup_returns_none_without_database() {
        // No file at this path → reader stays None → graceful miss.
        let geo = GeoIp::new(PathBuf::from("/nonexistent/geoip-test.BIN"));
        assert!(!geo.is_loaded());
        assert_eq!(geo.lookup("8.8.8.8"), None);
        // Garbage / non-routable inputs are also a clean miss.
        assert_eq!(geo.lookup("not-an-ip"), None);
        assert_eq!(geo.lookup("127.0.0.1"), None);
    }

    #[test]
    fn client_ip_prefers_xff_then_real_ip() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), None);

        h.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&h), Some("203.0.113.9".into()));

        h.insert(
            "x-forwarded-for",
            "198.51.100.7, 10.0.0.1, 203.0.113.1".parse().unwrap(),
        );
        // Left-most XFF (the original client) wins over X-Real-IP.
        assert_eq!(client_ip(&h), Some("198.51.100.7".into()));

        h.insert("cf-connecting-ip", "192.0.2.44".parse().unwrap());
        // Cloudflare header takes precedence when present.
        assert_eq!(client_ip(&h), Some("192.0.2.44".into()));
    }

    #[test]
    fn transport_secure_detects_https_localhost_and_public_url() {
        let http = "http://gateway.example.com";
        let https = "https://gateway.example.com";

        // X-Forwarded-Proto from a TLS-terminating proxy.
        let mut xfp = HeaderMap::new();
        xfp.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(transport_is_secure(&xfp, http));

        // Loopback Host → secure even on plain http with no XFP.
        let mut local = HeaderMap::new();
        local.insert("host", "localhost:8080".parse().unwrap());
        assert!(transport_is_secure(&local, http));
        let mut loop4 = HeaderMap::new();
        loop4.insert("host", "127.0.0.1:8080".parse().unwrap());
        assert!(transport_is_secure(&loop4, http));

        // Plain-HTTP LAN origin → NOT secure (the case that was futilely prompting).
        let mut lan = HeaderMap::new();
        lan.insert("host", "192.168.1.5:8080".parse().unwrap());
        assert!(!transport_is_secure(&lan, http));

        // …unless the operator declared an HTTPS public_url (proxy w/o XFP).
        assert!(transport_is_secure(&lan, https));

        // No headers at all, http public_url → insecure.
        assert!(!transport_is_secure(&HeaderMap::new(), http));
    }

    #[test]
    fn client_ip_rejects_garbage() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "garbage, 1.2.3.4".parse().unwrap());
        assert_eq!(client_ip(&h), None);
    }
}
