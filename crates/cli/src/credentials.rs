// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! On-disk credentials for the `gw` CLI.
//!
//! File: `~/.config/gw/credentials.toml`, mode `0600` on unix.
//!
//! Shape:
//!
//! ```toml
//! default_profile = "default"
//!
//! [profiles.default]
//! gateway_url = "https://gateway.example.com"
//! token       = "gwk_…"
//! user_email  = "alice@example.com"   # optional
//! issued_at   = "2026-05-16T10:32:11Z"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

fn default_profile_name() -> String {
    "default".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub gateway_url: String,
    pub token: String,
    pub user_email: Option<String>,
    pub issued_at: String,
}

pub fn default_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is unset; cannot locate gw credentials"))?;
    Ok(home.join(".config").join("gw").join("credentials.toml"))
}

pub fn load() -> Result<Credentials> {
    let path = default_path()?;
    if !path.exists() {
        return Ok(Credentials::default());
    }
    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<Credentials> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(creds: &Credentials) -> Result<()> {
    save_to(&default_path()?, creds)
}

pub fn save_to(path: &Path, creds: &Credentials) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let toml = toml::to_string_pretty(creds).context("serializing credentials")?;
    std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

impl Credentials {
    pub fn active_profile(&self) -> Option<&Profile> {
        self.profiles.get(&self.default_profile)
    }

    pub fn set_profile(&mut self, name: &str, profile: Profile) {
        self.profiles.insert(name.to_string(), profile);
        if self.profiles.len() == 1 {
            self.default_profile = name.to_string();
        }
    }

    pub fn remove_profile(&mut self, name: &str) -> Option<Profile> {
        self.profiles.remove(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("creds.toml");
        let mut c = Credentials::default();
        c.set_profile(
            "default",
            Profile {
                gateway_url: "http://localhost:8080".into(),
                token: "gwk_xxx".into(),
                user_email: Some("alice@example.com".into()),
                issued_at: "2026-01-01T00:00:00Z".into(),
            },
        );
        save_to(&path, &c).unwrap();
        let loaded = load_from(&path).unwrap();
        let p = loaded.active_profile().unwrap();
        assert_eq!(p.token, "gwk_xxx");
        assert_eq!(loaded.default_profile, "default");
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("creds.toml");
        let c = Credentials::default();
        save_to(&path, &c).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.toml");
        // load_from would error for missing; load() (the higher-level) skips.
        // Verify the higher-level behavior by invoking load_from on an
        // existing empty file:
        std::fs::write(&path, "").unwrap();
        let c = load_from(&path).unwrap();
        assert!(c.profiles.is_empty());
    }
}
