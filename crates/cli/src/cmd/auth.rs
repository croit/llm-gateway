// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `gw auth …` subcommands. The whole login dance — PKCE pair, /auth/cli/start,
//! browser-open, /auth/cli/poll loop — lives here.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;

use crate::client::GatewayClient;
use crate::credentials::{self, Profile};
use crate::pkce;

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Browser-based login. Opens your default browser to the gateway's OIDC
    /// provider, then receives a gateway API token here.
    Login {
        /// Don't try to open a browser; print the URL instead.
        #[arg(long)]
        no_browser: bool,
        /// Profile to save the credentials under. Defaults to `default`.
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Show the currently-authenticated user (queries the gateway).
    Whoami,
    /// Revoke the local token on the gateway and forget it locally.
    Logout,
    /// List the tools the gateway will inject on behalf of your role(s).
    Tools,
}

pub async fn run(cmd: AuthCmd, gateway_url: String) -> Result<()> {
    match cmd {
        AuthCmd::Login {
            no_browser,
            profile,
        } => login(gateway_url, &profile, no_browser).await,
        AuthCmd::Whoami => whoami(gateway_url).await,
        AuthCmd::Logout => logout(gateway_url).await,
        AuthCmd::Tools => tools(gateway_url).await,
    }
}

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const POLL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

async fn login(gateway_url: String, profile: &str, no_browser: bool) -> Result<()> {
    let client = GatewayClient::new(gateway_url.clone());
    let (verifier, challenge) = pkce::new_pair();

    let start = client
        .auth_cli_start(&challenge)
        .await
        .context("starting CLI login at the gateway")?;

    eprintln!("→ Opening sign-in page in your browser:");
    eprintln!("  {}", start.login_url);
    if !no_browser && let Err(err) = webbrowser::open(&start.login_url) {
        eprintln!("(could not auto-open the browser: {err}; please open it manually)");
    }
    eprintln!("  Waiting for sign-in (5m timeout)…");

    let deadline = std::time::Instant::now() + POLL_TIMEOUT;
    let token = loop {
        match client.auth_cli_poll(&start.state, &verifier).await {
            Ok(Some(t)) => break t,
            Ok(None) => {}
            Err(e) => return Err(e.context("polling for sign-in result")),
        }
        if std::time::Instant::now() > deadline {
            bail!("sign-in didn't complete within {POLL_TIMEOUT:?}; please retry");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    // Quickly identify ourselves so the credentials file records the user.
    let bearer_client = client.clone().with_bearer(&token);
    let user_email = bearer_client.me().await.ok().map(|me| me.email);

    let mut creds = credentials::load().unwrap_or_default();
    creds.set_profile(
        profile,
        Profile {
            gateway_url,
            token,
            user_email: user_email.clone(),
            issued_at: jiff::Timestamp::now().to_string(),
        },
    );
    credentials::save(&creds).context("saving credentials")?;

    match user_email {
        Some(email) => eprintln!("✓ Signed in as {email}"),
        None => eprintln!("✓ Signed in"),
    }
    let path = credentials::default_path()?;
    eprintln!("  Token stored in {}", path.display());
    Ok(())
}

async fn whoami(gateway_url: String) -> Result<()> {
    let creds = credentials::load()?;
    let profile = creds
        .active_profile()
        .ok_or_else(|| anyhow!("no active profile; run `gw auth login` first"))?;

    // Honour --gateway override but default to the saved profile URL.
    let url = if gateway_url == "http://localhost:8080" && profile.gateway_url != gateway_url {
        profile.gateway_url.clone()
    } else {
        gateway_url
    };
    let client = GatewayClient::new(url).with_bearer(&profile.token);
    let me = client.me().await.context("calling /v1/me")?;
    println!("id:    {}", me.id);
    println!("email: {}", me.email);
    if let Some(name) = me.name {
        println!("name:  {name}");
    }
    if me.roles.is_empty() {
        println!("roles: (none)");
    } else {
        println!("roles: {}", me.roles.join(", "));
    }
    Ok(())
}

async fn tools(gateway_url: String) -> Result<()> {
    let creds = credentials::load()?;
    let profile = creds
        .active_profile()
        .ok_or_else(|| anyhow!("no active profile; run `gw auth login` first"))?;

    let url = if gateway_url == "http://localhost:8080" && profile.gateway_url != gateway_url {
        profile.gateway_url.clone()
    } else {
        gateway_url
    };
    let client = GatewayClient::new(url).with_bearer(&profile.token);
    let me = client.me().await.context("calling /v1/me")?;
    if me.allowed_tools.is_empty() {
        println!("(no tools granted to your role)");
        return Ok(());
    }
    for tool in me.allowed_tools {
        println!("{id}", id = tool.id);
        println!("  {desc}", desc = tool.description);
        if tool.name != tool.id {
            println!("  name: {name}", name = tool.name);
        }
        println!();
    }
    Ok(())
}

async fn logout(gateway_url: String) -> Result<()> {
    let mut creds = credentials::load()?;
    let Some(profile) = creds.active_profile().cloned() else {
        eprintln!("Already signed out (no credentials on disk).");
        return Ok(());
    };

    // Best-effort server-side revocation. We delete the local copy either way.
    let url = if gateway_url == "http://localhost:8080" && profile.gateway_url != gateway_url {
        profile.gateway_url.clone()
    } else {
        gateway_url
    };
    let client = GatewayClient::new(url).with_bearer(&profile.token);
    match client.logout().await {
        Ok(_) => eprintln!("✓ Token revoked on the gateway."),
        Err(err) => eprintln!("(could not revoke token on the gateway: {err}; forgetting locally)"),
    }

    let name = creds.default_profile.clone();
    creds.remove_profile(&name);
    credentials::save(&creds).context("saving credentials")?;
    eprintln!("✓ Local credentials cleared.");
    Ok(())
}
