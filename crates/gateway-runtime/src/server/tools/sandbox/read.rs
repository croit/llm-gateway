// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

pub(crate) const READ_DEFAULT_LIMIT: usize = 200;
pub(crate) const READ_MAX_LIMIT: usize = 2000;
pub(crate) const READ_DEFAULT_MAX_BYTES: usize = 16 * 1024;
pub(crate) const READ_HARD_MAX_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReadAction {
    Grep,
    Head,
    Tail,
    Range,
}

pub(crate) fn default_read_action() -> ReadAction {
    ReadAction::Head
}

#[derive(Deserialize)]
pub(crate) struct ReadArgs {
    /// `full_output_ref` from a run_in_sandbox result, e.g. "<turn>/stdout.txt".
    id: String,
    #[serde(default = "default_read_action")]
    action: ReadAction,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// Reads slices of a stored sandbox output on demand, so the model can drill
/// into a large stdout/stderr (or any produced text file) without inlining
/// the whole thing. Chat-path only — it resolves the `full_output_ref` against
/// the current turn's attachments; API callers fetch `full_output_url` instead.
pub struct ReadSandboxOutput;

impl Tool for ReadSandboxOutput {
    fn id(&self) -> &str {
        "read_sandbox_output"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Read part of a large output a previous run_in_sandbox produced \
             (the value of its `full_output_ref`). Use this to drill into big \
             logs/results without pulling the whole thing into context: grep \
             for matching lines, or page through with head/tail/range.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {"type": "string", "description": "The full_output_ref from a run_in_sandbox result."},
                    "action": {"type": "string", "enum": ["grep", "head", "tail", "range"],
                               "description": "grep matching lines (needs `query`), or head/tail/range. Default head."},
                    "query": {"type": "string", "description": "Regex for action=grep."},
                    "start_line": {"type": "integer", "description": "1-based first line for action=range."},
                    "end_line": {"type": "integer", "description": "1-based last line for action=range."},
                    "limit": {"type": "integer", "description": "Max lines to return (default 200)."},
                    "max_bytes": {"type": "integer", "description": "Max bytes to return (default 16384)."}
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ReadArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, action?, …}}: {e}")))?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed("attachment storage not configured ([chat.s3])".into())
            })?;
            // `id` is "<turn>/<filename>". Restrict to the CURRENT turn so the
            // model can only read outputs it just produced — never another
            // conversation's attachments.
            let (turn, filename) = args
                .id
                .split_once('/')
                .ok_or_else(|| ToolError::InvalidArgs("id must be \"<turn>/<filename>\"".into()))?;
            match ctx.assistant_turn_id.as_deref() {
                Some(cur) if cur == turn => {}
                Some(_) | None => {
                    return Err(ToolError::Failed(
                        "read_sandbox_output can only read outputs from the current chat turn"
                            .into(),
                    ));
                }
            }
            let fetched = chat_attachments::fetch(s3, turn, filename)
                .await
                .map_err(|e| ToolError::Failed(format!("fetch output: {e}")))?;
            let text = String::from_utf8_lossy(&fetched.bytes);
            let limit = args
                .limit
                .unwrap_or(READ_DEFAULT_LIMIT)
                .clamp(1, READ_MAX_LIMIT);
            let max_bytes = args
                .max_bytes
                .unwrap_or(READ_DEFAULT_MAX_BYTES)
                .clamp(256, READ_HARD_MAX_BYTES);
            slice_text(
                &text,
                args.action,
                args.query.as_deref(),
                args.start_line,
                args.end_line,
                limit,
                max_bytes,
            )
        })
    }
}

/// Pure slicing over a stored text output. Returns a model-facing JSON object;
/// kept free of I/O so it's unit-testable.
pub(crate) fn slice_text(
    text: &str,
    action: ReadAction,
    query: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    limit: usize,
    max_bytes: usize,
) -> Result<Value, ToolError> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let numbered = |i: usize| (i + 1, lines[i]);

    // `window_total` = how many lines the action covers in the whole file
    // (before the `limit`), so `more_available` can tell the model there's
    // more to page through even when `selected` was capped.
    let (selected, window_total, matched_total): (Vec<(usize, &str)>, usize, Option<usize>) =
        match action {
            ReadAction::Head => ((0..total).take(limit).map(numbered).collect(), total, None),
            ReadAction::Tail => {
                let from = total.saturating_sub(limit);
                ((from..total).map(numbered).collect(), total, None)
            }
            ReadAction::Range => {
                let s = start_line.unwrap_or(1).max(1);
                let e = end_line.unwrap_or(s + limit - 1).max(s);
                let lo = s.min(total + 1);
                let hi = e.min(total);
                let window = (hi + 1).saturating_sub(lo); // count of lines in [s,e]
                let sel: Vec<(usize, &str)> = (lo..=hi)
                    .take(limit)
                    .map(|ln| (ln, lines[ln - 1]))
                    .collect();
                (sel, window, None)
            }
            ReadAction::Grep => {
                let q =
                    query.ok_or_else(|| ToolError::InvalidArgs("grep requires `query`".into()))?;
                let re = regex::Regex::new(q)
                    .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
                let all: Vec<(usize, &str)> = (0..total)
                    .map(numbered)
                    .filter(|(_, l)| re.is_match(l))
                    .collect();
                let matched = all.len();
                (
                    all.into_iter().take(limit).collect(),
                    matched,
                    Some(matched),
                )
            }
        };

    let mut content = String::new();
    let mut returned = 0usize;
    let mut byte_capped = false;
    for (ln, l) in &selected {
        let piece = format!("{ln}: {l}\n");
        if !content.is_empty() && content.len() + piece.len() > max_bytes {
            byte_capped = true;
            break;
        }
        content.push_str(&piece);
        returned += 1;
    }
    let more = byte_capped || returned < window_total;
    Ok(json!({
        "total_lines": total,
        "returned_lines": returned,
        "matched_lines": matched_total,
        "more_available": more,
        "content": content,
    }))
}

// ---------------------------------------------------------------------------
// Minimal base64 (standard, padded). Encode for input files, decode for
// artifacts. Self-contained to keep the tool off a base64 dependency,
// matching the codecs in `chat_attachments` / `upload_attachment`.

pub mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(if chunk.len() >= 2 {
                ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() >= 3 {
                ALPHABET[(b2 & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u8> {
            Some(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            })
        }
        let mut quad = [0u8; 4];
        let mut qn = 0usize;
        let mut pads = 0usize;
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        for &c in s.as_bytes() {
            if c.is_ascii_whitespace() {
                continue;
            }
            if c == b'=' {
                quad[qn] = 0;
                pads += 1;
            } else {
                if pads > 0 {
                    return None;
                }
                quad[qn] = val(c)?;
            }
            qn += 1;
            if qn == 4 {
                out.push((quad[0] << 2) | (quad[1] >> 4));
                if pads < 2 {
                    out.push((quad[1] << 4) | (quad[2] >> 2));
                }
                if pads < 1 {
                    out.push((quad[2] << 6) | quad[3]);
                }
                qn = 0;
                if pads > 0 {
                    break;
                }
            }
        }
        if qn != 0 { None } else { Some(out) }
    }
}
