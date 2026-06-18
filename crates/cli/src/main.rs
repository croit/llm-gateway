// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use anyhow::Context;
use clap::Parser;

use cli::client::GatewayClient;
use cli::parser::{Cli, Command};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("gw: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,cli=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Cli::parse();

    let gateway_url = args
        .gateway
        .unwrap_or_else(|| "http://localhost:8080".into());

    match args.command {
        Command::Ping => {
            let client = GatewayClient::new(gateway_url);
            client
                .healthz()
                .await
                .with_context(|| format!("ping {}", client.base_url()))?;
            println!("ok");
            Ok(())
        }
        Command::Auth(cmd) => cli::cmd::auth::run(cmd, gateway_url).await,
    }
}
