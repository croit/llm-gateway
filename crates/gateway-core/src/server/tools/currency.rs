// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `convert_currency` — convert an amount between currencies using the
//! keyless Frankfurter API (daily ECB reference rates). Read-only, public,
//! no secrets, no writes. LLMs guess exchange rates badly; this gives a live
//! (if once-daily) figure with the rate and reference date.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const USER_AGENT: &str = concat!("llm-gateway/", env!("CARGO_PKG_VERSION"), " currency");

pub struct ConvertCurrency;

#[derive(Deserialize)]
struct Args {
    /// Amount to convert. Defaults to 1.
    #[serde(default)]
    amount: Option<f64>,
    from: String,
    to: String,
}

/// ISO 4217 codes are three ASCII letters. Normalises + validates so we only
/// ever interpolate a clean code into the request.
fn normalize_code(raw: &str) -> Result<String, ToolError> {
    let code = raw.trim().to_ascii_uppercase();
    if code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Ok(code)
    } else {
        Err(ToolError::InvalidArgs(format!(
            "`{raw}` is not a 3-letter currency code (e.g. USD, EUR, GBP)"
        )))
    }
}

impl Tool for ConvertCurrency {
    fn id(&self) -> &str {
        "convert_currency"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Convert an amount from one currency to another using daily ECB reference rates \
             (~30 major currencies: USD, EUR, GBP, JPY, CHF, …). Returns the converted amount, \
             the exchange rate, and the rate's reference date. Use this instead of guessing \
             exchange rates. Rates update once per business day, not intraday.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["from", "to"],
                "properties": {
                    "amount": { "type": "number", "exclusiveMinimum": 0, "description": "Amount to convert. Defaults to 1." },
                    "from": { "type": "string", "description": "Source currency, ISO 4217 code, e.g. \"USD\"." },
                    "to": { "type": "string", "description": "Target currency, ISO 4217 code, e.g. \"EUR\"." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{amount?, from, to}}: {e}"))
            })?;
            let amount = args.amount.unwrap_or(1.0);
            if !amount.is_finite() || amount <= 0.0 {
                return Err(ToolError::InvalidArgs(
                    "`amount` must be a positive, finite number".into(),
                ));
            }
            let from = normalize_code(&args.from)?;
            let to = normalize_code(&args.to)?;

            // Same currency: no API call needed.
            if from == to {
                return Ok(json!({
                    "amount": amount, "from": from, "to": to,
                    "converted": amount, "rate": 1.0,
                    "note": "Source and target currency are the same.",
                }));
            }

            let client = reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))?;
            let resp = client
                .get("https://api.frankfurter.app/latest")
                .query(&[
                    ("amount", amount.to_string().as_str()),
                    ("from", from.as_str()),
                    ("to", to.as_str()),
                ])
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("rate request failed: {e}")))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND
                || resp.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY
            {
                return Err(ToolError::InvalidArgs(format!(
                    "unsupported currency code ({from} or {to} not in the ECB rate set)"
                )));
            }
            if !resp.status().is_success() {
                return Err(ToolError::Failed(format!(
                    "rate service returned {}",
                    resp.status()
                )));
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Failed(format!("rate response parse: {e}")))?;

            // `{ "amount": N, "base": "USD", "date": "…", "rates": { "EUR": M } }`
            let converted = body
                .get("rates")
                .and_then(|r| r.get(&to))
                .and_then(Value::as_f64)
                .ok_or_else(|| ToolError::Failed(format!("no rate returned for {from}→{to}")))?;
            let date = body.get("date").and_then(Value::as_str);

            Ok(json!({
                "amount": amount,
                "from": from,
                "to": to,
                "converted": converted,
                "rate": converted / amount,
                "as_of": date,
                "note": "Daily ECB reference rate (not intraday).",
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_name_matches_id() {
        assert_eq!(ConvertCurrency.id(), ConvertCurrency.schema().function.name);
    }

    #[test]
    fn normalize_code_accepts_and_rejects() {
        assert_eq!(normalize_code(" usd ").unwrap(), "USD");
        assert_eq!(normalize_code("eur").unwrap(), "EUR");
        assert!(normalize_code("US").is_err());
        assert!(normalize_code("US1").is_err());
        assert!(normalize_code("dollar").is_err());
    }
}
