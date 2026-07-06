// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Types shared between the `gateway` server and the `gw` CLI / web UI.
//!
//! This crate is intentionally I/O-free — pure data types only. Everything
//! that crosses an API boundary lives here so the wire format stays in one
//! place.

pub mod api;
pub mod sandbox;

/// Healthcheck response body shape. Tiny, but having it here keeps the gateway
/// and CLI from drifting on string literals.
pub const HEALTHZ_BODY: &str = "ok";

/// A short, human-comparable confirmation code derived from a CLI login
/// `state`. `gw auth login` prints it in the terminal, and the browser
/// approval page shows the same value — so the user can confirm the
/// browser prompt belongs to the login *they* started, and spot a phished
/// prompt for a login they never initiated. Deterministic and
/// dependency-free (FNV-1a folded to 32 bits); it is a display aid, not a
/// secret, and lives here so both sides compute it identically.
pub fn cli_login_code(state: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in state.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let s = format!("{:08X}", (hash & 0xffff_ffff) as u32);
    format!("{}-{}", &s[..4], &s[4..])
}

#[cfg(test)]
mod tests {
    use super::cli_login_code;

    #[test]
    fn cli_login_code_is_deterministic_and_shaped() {
        let a = cli_login_code("abc123");
        assert_eq!(a, cli_login_code("abc123"), "must be deterministic");
        assert_eq!(a.len(), 9, "XXXX-XXXX");
        assert_eq!(a.as_bytes()[4], b'-');
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "hex + dash only: {a}"
        );
        // Different states almost always differ (sanity, not a guarantee).
        assert_ne!(cli_login_code("state-a"), cli_login_code("state-b"));
    }
}
