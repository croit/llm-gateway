// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The `gw` command-line surface (clap derive). Lives in the lib — not
//! `main.rs` — so tests can introspect the command tree via
//! `clap::CommandFactory` (see `tests/readme_cli.rs`, which pins it to the
//! README command table).

use clap::{Parser, Subcommand};

use crate::cmd::auth::AuthCmd;

#[derive(Parser, Debug)]
#[command(
    name = "gw",
    version,
    about = "LLM gateway CLI",
    long_about = "Talk to the LLM gateway: authenticate, list models, ping. \
                  See `docs/cli.md` for the full command reference."
)]
pub struct Cli {
    /// Gateway base URL (overrides $GW_GATEWAY_URL and the saved profile).
    #[arg(long, global = true, env = "GW_GATEWAY_URL")]
    pub gateway: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Verify the gateway is reachable. Hits `GET /healthz`.
    Ping,
    /// Authentication: login, whoami, logout.
    #[command(subcommand)]
    Auth(AuthCmd),
}
