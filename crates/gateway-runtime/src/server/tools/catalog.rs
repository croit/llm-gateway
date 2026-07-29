// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The user-facing tool catalog: turns the flat `ToolRegistry` into the
//! grouped, de-noised list the `/tools` page renders, and provides the
//! mapping that the request path uses to honour a user's on/off
//! choices.
//!
//! Two concerns live here so they can't drift apart:
//!   - **Display**: [`entries`] groups tools into [`Category`]s, hides
//!     smoke-test-only tools, and folds the per-template `typst_<id>`
//!     family into a single "Document rendering" toggle.
//!   - **Enforcement**: [`entry_key_for`] / [`retain_enabled`] map a
//!     registered tool id to its toggle key and drop the ids whose key
//!     the user disabled. The page and the proxy therefore agree on
//!     exactly what one toggle controls.

use std::collections::HashSet;

use super::ToolRegistry;

pub use gateway_core::server::tool_naming::{
    BOOTSTRAP_TOOL_ID, COMFYUI_KEY, COMFYUI_PREFIX, READ_SKILL_ID, TYPST_PREFIX, prettify,
};

/// Tool-id suffixes of the per-template variant tools. `entry_key_for` strips
/// these to recover the template's render id (its toggle key); discovery
/// rejects template ids ending in any of them so the strip is unambiguous.
const TYPST_VARIANT_SUFFIXES: &[&str] = &["_edit", "_read", "_pptx"];

/// The memory tools are facets of one capability, so they collapse to a
/// single "memory" toggle — one switch turns per-user memory on or off as a
/// whole. `update_memory` and `forget` belong on the same switch as
/// `remember`: a user who turns memory off must not be left with tools that
/// can still mutate the store, and one who turns it on needs the store to be
/// correctable, not append-only.
const MEMORY_IDS: &[&str] = &["remember", "recall", "update_memory", "forget"];
const MEMORY_KEY: &str = "memory";

/// The document-canvas tools are one capability — building up and editing
/// a long document across turns — so they collapse to a single "document"
/// toggle. An explicit id list (rather than a substring match) so the
/// sandbox's `generate_document` / `convert_document` stay in their own
/// "Code & Sandbox" group.
const DOCUMENT_IDS: &[&str] = &[
    // `import_file` only exists to *make* a canvas document (out of a file the
    // conversation already holds), so it belongs on the canvas switch: with
    // the canvas off there is nothing it could produce that anything reads.
    "import_file",
    "create_document",
    "edit_document",
    "read_document",
    "list_documents",
    "export_document",
    "edit_document_section",
    "list_document_versions",
    "restore_document_version",
    "delete_document",
    "undelete_document",
];
const DOCUMENT_KEY: &str = "document";

/// The scheduled-action tools are one capability — "let the assistant manage
/// my recurring prompts" — so they collapse to a single toggle. Splitting them
/// would let a user grant `list` without `create`, which is not a distinction
/// anyone reasons about, and would leave the model able to see a schedule it
/// cannot change.
const SCHEDULE_IDS: &[&str] = &[
    "schedule_action",
    "list_scheduled_actions",
    "delete_scheduled_action",
];
const SCHEDULE_KEY: &str = "schedule";

/// Attaching a file to a reply is one capability with two halves — compose
/// new content (`upload_attachment`) or hand over something the
/// conversation already holds (`offer_download`) — so they share a toggle.
/// A user who turned "attach files to replies" on means both; leaving the
/// key as the older id keeps existing `user_tool_prefs` rows meaningful.
const ATTACH_IDS: &[&str] = &["upload_attachment", "offer_download"];
const ATTACH_KEY: &str = "upload_attachment";

/// Tools that exist for smoke tests / internal plumbing and shouldn't
/// clutter a user's tool list. They stay granted via RBAC; they're just
/// not presented as a toggle.
///
/// `enable_tools` belongs here because its toggle could never do anything:
/// `AppState::allowed_tools_for_session` force-keeps [`BOOTSTRAP_TOOL_ID`]
/// regardless of the per-conversation enabled set, so a switch for it would
/// be inert — and it rendered with its model-facing description, since
/// internal plumbing has no hand-written display copy. `capability_domains`
/// already skipped it; `entries` did not.
const HIDDEN: &[&str] = &["company_echo", BOOTSTRAP_TOOL_ID];

/// Display grouping for the tool list. Ordered by [`Category::order`]
/// so the page renders sections deterministically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Web,
    Documents,
    /// Image generation / editing (`generate_image`, `edit_image`).
    Media,
    /// Per-template typst document tools (`typst_<id>`) — one row per
    /// operator-installed template, individually selectable.
    Templates,
    /// ComfyUI workflows (`comfyui_<id>`) — image / video / audio
    /// generation through the headless ComfyUI worker. Distinct from
    /// [`Category::Media`] (the OpenAI-compatible image tools) so users
    /// see which backend a toggle actually governs.
    ComfyMedia,
    /// Semantic search over operator-indexed knowledge bases (`rag_*`) —
    /// the user's own repositories and documents.
    Knowledge,
    /// Sandboxed code execution (`run_in_sandbox` + its presets).
    Code,
    Memory,
    /// Recurring prompts the user has scheduled (`schedule_action` and
    /// friends) — a capability area of its own because the thing being
    /// configured outlives the conversation that configured it.
    Scheduling,
    /// Tools bridged from an external MCP server (`mcp__*`).
    Integrations,
    Utility,
}

impl Category {
    /// Section heading shown on the page.
    pub fn label(self) -> &'static str {
        match self {
            Category::Web => "Web & Network",
            Category::Documents => "Attachments & Documents",
            Category::Media => "Images & Media",
            Category::Templates => "Document templates",
            Category::ComfyMedia => "ComfyUI workflows",
            Category::Knowledge => "Knowledge base",
            Category::Code => "Code & Sandbox",
            Category::Memory => "Memory",
            Category::Scheduling => "Scheduled actions",
            Category::Integrations => "Integrations",
            Category::Utility => "Utility",
        }
    }

    /// Render order — lower sorts first.
    pub fn order(self) -> u8 {
        match self {
            Category::Web => 0,
            Category::Documents => 1,
            Category::Media => 2,
            Category::ComfyMedia => 3,
            Category::Templates => 4,
            Category::Knowledge => 5,
            Category::Code => 6,
            Category::Memory => 7,
            Category::Scheduling => 8,
            Category::Integrations => 9,
            Category::Utility => 10,
        }
    }
}

/// One row on the `/tools` page. `key` is the stable toggle identity
/// persisted in `user_tool_prefs`; `title` is the human-readable name,
/// `tech` the underlying function name shown as a subtle mono badge,
/// and `description` the plain-language "what + how".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolEntry {
    pub key: String,
    pub title: String,
    pub tech: String,
    pub description: String,
    pub category: Category,
}

/// Display metadata for one discovered typst template, snapshotted at startup
/// (the human `title` lives in the manifest, not the tool schema, so the
/// catalog can't recover it from the registry alone). `key` is the
/// per-template toggle key — the render id `typst_<id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateMeta {
    pub key: String,
    pub title: String,
    pub description: String,
}

/// Same shape as [`TemplateMeta`], but for ComfyUI workflows (`comfyui_<id>`).
/// `tool_id` is the full tool id including the `comfyui_` prefix; `key` would
/// be redundant so the field is named differently to make call sites read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComfyuiMeta {
    pub tool_id: String,
    pub title: String,
    pub description: String,
}

/// The toggle key that governs a registered tool id. Each typst *template*
/// is its own key: `typst_<id>` plus its `_edit`/`_read`/`_pptx` variants all
/// collapse to `typst_<id>` (the render id), so one switch governs that
/// template's whole family while different templates stay independent. Every
/// other tool is its own key. Pure + cheap so the request path can call it per
/// id without touching the DB.
pub fn entry_key_for(tool_id: &str) -> &str {
    if tool_id.starts_with(TYPST_PREFIX) {
        // Strip a variant suffix to recover the template's render id. Template
        // ids can't end in these (rejected at discovery), so this is exact.
        TYPST_VARIANT_SUFFIXES
            .iter()
            .find_map(|s| tool_id.strip_suffix(s))
            .unwrap_or(tool_id)
    } else if MEMORY_IDS.contains(&tool_id) {
        MEMORY_KEY
    } else if ATTACH_IDS.contains(&tool_id) {
        ATTACH_KEY
    } else if DOCUMENT_IDS.contains(&tool_id) {
        DOCUMENT_KEY
    } else if SCHEDULE_IDS.contains(&tool_id) {
        SCHEDULE_KEY
    } else if tool_id.starts_with(COMFYUI_PREFIX) {
        // All comfyui_* tools collapse to the single "comfyui" toggle,
        // so the user enables/disables the whole family at once.
        COMFYUI_KEY
    } else if tool_id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
        // All of one MCP server's tools collapse to a single toggle, so a
        // user enables/disables the whole integration at once.
        mcp_server_key(tool_id)
    } else {
        tool_id
    }
}

/// Whether a tool id is hidden from every user-facing surface — both the
/// `/tools` settings page and the model-facing `enable_tools` catalog.
/// Smoke-test / internal-plumbing tools stay granted via RBAC; they're just
/// never advertised as a toggle. Single source of truth so the page and the
/// bootstrap tool can't drift on what counts as hidden.
pub fn is_hidden(tool_id: &str) -> bool {
    HIDDEN.contains(&tool_id)
}

/// Tools that only work inside a chat session — their `run` hard-fails
/// (`"… only available inside a chat session"`) off the chat path because
/// there is no assistant turn / conversation to attach their output to:
/// the per-template typst render family (`typst_*`), the document-canvas
/// tools (incl. `export_document`), `upload_attachment`, the image tools
/// (`generate_image` / `edit_image`), and `read_sandbox_output` (it resolves
/// a `full_output_ref` against the *current turn's* attachments). The `/v1`
/// proxy paths have no session, so they must NOT advertise these — else the
/// model picks one and gets an error instead of a completion. Single source
/// of truth so the advertise filter can't drift from the runtime gate.
///
/// Deliberately *not* listed: `render_typst` needs a session only when the
/// call references a canvas document, which is one optional argument among
/// several — off-chat it still renders model-supplied source. Same for the
/// sandbox tools that merely *prefer* a turn to attach files to: they fall
/// back to returning a URL instead of an attachment ref.
pub fn requires_chat_session(tool_id: &str) -> bool {
    tool_id.starts_with(TYPST_PREFIX)
        || tool_id.starts_with(COMFYUI_PREFIX)
        || DOCUMENT_IDS.contains(&tool_id)
        || tool_id == "upload_attachment"
        // Takes a file *from* the conversation and attaches it *to* the
        // current reply — both halves need a live chat turn.
        || tool_id == "offer_download"
        || tool_id == "list_attachments"
        || tool_id == "generate_image"
        || tool_id == "edit_image"
        || tool_id == "generate_qr_code"
        || tool_id == "load_image_url"
        || tool_id == "read_sandbox_output"
        // Needs a live turn *and* a human watching it to answer.
        || tool_id == "ask_user"
        // Creating or deleting a scheduled action needs a human "yes" (the
        // action later runs *as the user*, unattended), and that confirmation
        // is an `ask_user` card — so these need the same live chat turn.
        // Listing is read-only and stays available off-chat.
        || tool_id == "schedule_action"
        || tool_id == "delete_scheduled_action"
}

/// `mcp__<server>__<tool>` → `mcp__<server>` (the per-server toggle key).
/// Falls back to the whole id if the shape is unexpected.
fn mcp_server_key(tool_id: &str) -> &str {
    let prefix = crate::server::tools::mcp::MCP_ID_PREFIX;
    let after = &tool_id[prefix.len()..];
    match after.find("__") {
        Some(i) => &tool_id[..prefix.len() + i],
        None => tool_id,
    }
}

/// Display category for a tool id. Unknown / future tools fall into
/// `Utility` and render as their own 1:1 row — a graceful default that
/// never hides a newly added tool.
///
/// Public so the tool-inventory drift guard can assert that no registered id
/// lands in `Utility` by accident. That is
/// exactly how `convert_document` and `edit_presentation` sat in the wrong
/// group with LLM-facing descriptions: the graceful default is also a silent
/// one, so it needs a test rather than a reviewer noticing.
pub fn category_for(tool_id: &str) -> Category {
    match tool_id {
        "search_web" | "fetch_url" | "lookup_ip" | "dns_lookup" | "whois_lookup" | "tls_cert"
        | "wikipedia" => Category::Web,
        "fetch_attachment" | "upload_attachment" | "offer_download" | "list_attachments"
        | "read_skill" => Category::Documents,
        "generate_image" | "edit_image" | "generate_qr_code" | "load_image_url" => Category::Media,
        "rag_search" | "rag_grep" | "rag_list_collections" => Category::Knowledge,
        "run_in_sandbox"
        | "generate_document"
        | "convert_document"
        | "edit_presentation"
        | "capture_webpage"
        | "browse_page"
        | "read_sandbox_output"
        | "render_excalidraw"
        | "render_typst"
        | "render_video" => Category::Code,
        _ if tool_id.starts_with(TYPST_PREFIX) => Category::Templates,
        _ if tool_id.starts_with(COMFYUI_PREFIX) => Category::ComfyMedia,
        _ if DOCUMENT_IDS.contains(&tool_id) => Category::Documents,
        // Against the list, not literals: adding a fifth memory tool must not
        // be able to categorise it as Utility by omission. (It could, until
        // `update_memory` and `forget` landed and the drift guard said so.)
        _ if MEMORY_IDS.contains(&tool_id) => Category::Memory,
        _ if SCHEDULE_IDS.contains(&tool_id) => Category::Scheduling,
        _ if tool_id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) => {
            Category::Integrations
        }
        // `get_user_location`, `ask_user`, `notify_user` and any future tool
        // fall here. The
        // inventory guard pins the expected set, so a *new* id landing here is
        // caught rather than silently accepted.
        _ => Category::Utility,
    }
}

/// First sentence of a model-facing tool description, for a compact UI
/// label. Falls back to the whole string when there's no sentence
/// break.
fn short_description(full: &str) -> String {
    match full.find(". ") {
        Some(end) => full[..=end].trim().to_string(),
        None => full.trim().to_string(),
    }
}

/// Whether `tool_id` has hand-written, user-facing display copy.
///
/// `false` means the `/tools` page falls back to the tool's *model-facing*
/// schema description, which reads as jargon in a settings list. The
/// inventory drift guard asserts this holds for every registered 1:1 row, so
/// a new tool can't ship with LLM prose in the UI.
pub fn has_display_copy(tool_id: &str) -> bool {
    display_meta(tool_id).is_some()
}

/// Curated, user-facing `(title, description)` for a known tool id. The
/// model-facing `schema().description` is written for the LLM and reads
/// terse / jargon-y in a settings list, so we hand-write plain-language
/// copy here. Unknown / future tools fall back to their schema text
/// (see [`entries`]).
fn display_meta(tool_id: &str) -> Option<(&'static str, &'static str)> {
    let meta = match tool_id {
        "search_web" => (
            "Web search",
            "Searches the web and returns a short list of results — each with a title, link, \
             and snippet. For current events, niche facts, or anything newer than the model's \
             training data.",
        ),
        "fetch_url" => (
            "Fetch a web page",
            "Loads a specific http(s) URL and returns its text (or image) so the assistant can \
             read and quote the actual page or file — instead of guessing.",
        ),
        "fetch_attachment" => (
            "Read an attachment",
            "Opens a file you attached to the chat and reads its contents, so the assistant can \
             summarise, quote, or work from it.",
        ),
        "upload_attachment" => (
            "Attach a file to replies",
            "Lets the assistant attach a file to its answer so you can download it — one it \
             generated (a document, image, or data export) as well as anything the \
             conversation already holds, including files from earlier turns and the data \
             behind a rendered document. Without this it can only paste content into the \
             reply as text.",
        ),
        "list_attachments" => (
            "List conversation files",
            "Lets the assistant see every file in the conversation — your uploads and files it \
             produced earlier — so it can reuse an existing document or image instead of \
             generating it again.",
        ),
        "get_current_timestamp" => (
            "Current date & time",
            "Gives the assistant today's date and the current time in your timezone — for \
             questions like \"what's due today\" or scheduling.",
        ),
        "notify_user" => (
            "Push notifications",
            "Lets the assistant send you a notification on your phone or desktop when you \
             are not watching the conversation — long work finished, or a scheduled action \
             found something you should know. Limited to one per reply, and it needs a \
             device you enabled notifications on.",
        ),
        "ask_user" => (
            "Clarifying questions",
            "Lets the assistant ask you a short question mid-answer — which option you \
             meant, which file to use — and wait for your reply, instead of guessing and \
             producing work you did not want.",
        ),
        "get_user_location" => (
            "Your location",
            "Lets the assistant figure out where you are — for \"weather here\", \"near me\", \
             and similar — from a precise location you share or, failing that, an approximate \
             one from your IP address.",
        ),
        "lookup_ip" => (
            "IP / host location",
            "Looks up where any IP address or hostname is — country, region, city, and rough \
             coordinates — from the gateway's offline IP2Location database, so the assistant \
             can answer \"where is this IP?\" without searching the web.",
        ),
        "dns_lookup" => (
            "DNS lookup",
            "Resolves DNS records for a hostname (addresses, mail servers, TXT, etc.) so the \
             assistant can answer \"what does this domain resolve to?\" with live data.",
        ),
        "whois_lookup" => (
            "Domain WHOIS",
            "Looks up a domain's registration details — registrar, creation/expiry dates, \
             status, nameservers — via RDAP (the modern WHOIS).",
        ),
        "tls_cert" => (
            "TLS certificate check",
            "Inspects the TLS certificate a site presents — issuer, validity dates, days until \
             expiry, and covered hostnames — so the assistant can answer \"is this cert about \
             to expire?\".",
        ),
        "wikipedia" => (
            "Wikipedia lookup",
            "Fetches the summary of the best-matching Wikipedia article for encyclopedic \
             \"who/what/where is X\" questions, with a link to the full page.",
        ),
        "generate_image" => (
            "Image generation",
            "Lets the assistant create an image from a text description — diagrams, mockups, \
             illustrations, or marketing visuals — and drop it straight into its reply for you \
             to download or reuse.",
        ),
        "load_image_url" => (
            "Load image from a URL",
            "Lets the assistant download an image from a web address and keep it as a reusable \
             file in the conversation — so it can embed that image in a generated document \
             (e.g. a logo in a letter) or reuse it in a later reply.",
        ),
        "edit_image" => (
            "Image editing",
            "Lets the assistant edit an image already in the conversation from a text \
             instruction — change the background, add or remove an element, restyle it — and \
             return the edited version inline.",
        ),
        "generate_qr_code" => (
            "QR codes",
            "Lets the assistant generate QR codes — for links, WiFi access, contact cards, \
             or SEPA payments — as PNG or SVG, with custom colors and an optional centered \
             logo, delivered straight into its reply.",
        ),
        "convert_currency" => (
            "Currency conversion",
            "Converts an amount between currencies using daily ECB reference rates, so the \
             assistant gives a real figure instead of guessing the exchange rate.",
        ),
        "rag_search" => (
            "Knowledge-base search",
            "Lets the assistant semantically search the repositories and documents indexed \
             into this gateway — your own codebase, docs, or data — and quote the matching \
             passages, instead of guessing or searching the public web.",
        ),
        "rag_grep" => (
            "Knowledge-base pattern search",
            "Lets the assistant search the indexed repositories with a regular expression and \
             get back the matching lines with file and line number — for \"every place that \
             looks like this\", where a meaning-based search can't express the pattern.",
        ),
        "rag_list_collections" => (
            "List knowledge bases",
            "Lets the assistant discover which indexed collections exist before searching \
             them, so it queries the right repository or document set.",
        ),
        "read_skill" => (
            "Skills",
            "Lets the assistant load an operator-installed skill — brand guidelines, house \
             style, domain playbooks — and apply it to what it writes or builds for you.",
        ),
        "run_in_sandbox" => (
            "Code sandbox",
            "Lets the assistant run Python or shell in a secure throwaway sandbox — for data \
             analysis, charts, calculations, running command-line tools, and generating \
             files it returns to you. Isolated per conversation; within one, the assistant \
             can run several steps that build on each other.",
        ),
        "generate_document" => (
            "Document generation",
            "Lets the assistant turn its writing into a finished PDF, Word, or PowerPoint \
             file you can download — built in the sandbox from Markdown.",
        ),
        "convert_document" => (
            "File conversion",
            "Lets the assistant convert a file you uploaded — PowerPoint, Word, Excel, ODF, \
             PDF — into PDF, Word, plain text, HTML, or one image per page, and hand the \
             result back to download.",
        ),
        "edit_presentation" => (
            "Edit a PowerPoint",
            "Lets the assistant change a PowerPoint file you uploaded and return the edited \
             deck — rewriting slide text or swapping images while keeping the original \
             layout and theme, instead of rebuilding the deck from scratch.",
        ),
        "browse_page" => (
            "Browse a website",
            "Lets the assistant drive a real browser across several steps — click a link, \
             fill a form, get past a consent banner, look at the next page — instead of only \
             being able to read a page once. Needs the operator to have enabled sandbox \
             network access.",
        ),
        "capture_webpage" => (
            "Web page capture",
            "Lets the assistant open a web page in a headless browser and hand you a \
             full-page screenshot, a PDF, or its extracted text. Needs operator-enabled \
             network access.",
        ),
        "read_sandbox_output" => (
            "Read large sandbox output",
            "Lets the assistant grep or page through a large result a previous sandbox \
             run produced, without pulling the whole thing back into the conversation.",
        ),
        "render_excalidraw" => (
            "Diagram rendering",
            "Lets the assistant turn an Excalidraw diagram — one it draws, or one you \
             upload — into an SVG, PNG, or PDF you can download or drop into slides.",
        ),
        "render_typst" => (
            "Charts & Typst documents",
            "Lets the assistant write a Typst document — ggplot-style charts, diagrams, or \
             slide decks, and embed images it made earlier — and hand you the finished \
             PDF, PNG, or SVG.",
        ),
        "render_video" => (
            "Video editing",
            "Lets the assistant cut clips together with transitions, add animated text and \
             a logo, and lay music underneath — for promo and ad videos. The timeline is a \
             JSON document you can edit by hand and re-render, so a revision keeps the same \
             look.",
        ),
        _ => return None,
    };
    Some(meta)
}

/// The distinct high-level capability *areas* the registry actually offers,
/// as user-facing category labels in display order. Drives the one-line
/// "here's what you can turn on" hint in the model's system context: domains,
/// not individual tools, so it stays cheap and the model still calls
/// `enable_tools` for the exact keys. Derived from the live registry, so a
/// deployment without (say) a sandbox or an indexer never advertises that
/// area. MCP integrations are excluded — they're listed per-user in the
/// system context already; hidden, bootstrap, and the skill loader don't
/// count toward an area on their own.
pub fn capability_domains(registry: &ToolRegistry) -> Vec<&'static str> {
    let mut seen: Vec<Category> = Vec::new();
    for id in registry.ids() {
        if is_hidden(id) || id == BOOTSTRAP_TOOL_ID || id == "read_skill" {
            continue;
        }
        if id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
            continue;
        }
        let cat = category_for(id);
        if cat == Category::Integrations {
            continue;
        }
        if !seen.contains(&cat) {
            seen.push(cat);
        }
    }
    seen.sort_by_key(|c| c.order());
    seen.into_iter().map(Category::label).collect()
}

/// Build the grouped, de-noised toggle list from the tool ids the
/// user's roles grant. Hidden tools are dropped; each typst *template* gets
/// its own row (its render + variant tools fold into one), labelled from
/// `templates` (the startup snapshot of manifest titles/descriptions).
/// ComfyUI workflows likewise get one row each, labelled from
/// `comfyui_metas` (live snapshot — hot-reloadable). Sorted by category
/// then key so the page is stable across requests.
pub fn entries(
    registry: &ToolRegistry,
    allowed: &[String],
    templates: &[TemplateMeta],
    comfyui_metas: &[ComfyuiMeta],
) -> Vec<ToolEntry> {
    let mut out: Vec<ToolEntry> = Vec::new();
    let mut typst_seen: HashSet<String> = HashSet::new();
    let mut comfyui_seen = false;
    let mut memory_seen = false;
    let mut document_seen = false;
    let mut schedule_seen = false;
    let mut mcp_servers_seen: HashSet<String> = HashSet::new();

    for id in allowed {
        if is_hidden(id) {
            continue;
        }
        if DOCUMENT_IDS.contains(&id.as_str()) {
            if !document_seen {
                document_seen = true;
                out.push(ToolEntry {
                    key: DOCUMENT_KEY.to_string(),
                    title: "Document canvas".to_string(),
                    tech: "create/edit/read/list_document".to_string(),
                    description: "Lets the assistant build up a long document (a guide, spec, or \
                                  config) in a live side panel and edit it one passage at a time \
                                  across turns — instead of rewriting the whole thing each reply. \
                                  A file you upload can be pulled into the panel the same way, so \
                                  it becomes editable rather than only readable. Keeps a full \
                                  version history the assistant can roll back to, and lets it \
                                  clear away drafts it no longer needs (deleted documents keep \
                                  their history and can be brought back)."
                        .to_string(),
                    category: Category::Documents,
                });
            }
            continue;
        }
        if MEMORY_IDS.contains(&id.as_str()) {
            if !memory_seen {
                memory_seen = true;
                out.push(ToolEntry {
                    key: MEMORY_KEY.to_string(),
                    title: "Memory".to_string(),
                    tech: MEMORY_IDS.join(" + "),
                    description: "Lets the assistant remember durable facts about you \
                                  (preferences, ongoing projects), recall them in later \
                                  conversations, and correct or delete one when it changes. \
                                  Stored only for your account; you can review and edit \
                                  everything on the Memory page."
                        .to_string(),
                    category: Category::Memory,
                });
            }
            continue;
        }
        if SCHEDULE_IDS.contains(&id.as_str()) {
            if !schedule_seen {
                schedule_seen = true;
                out.push(ToolEntry {
                    key: SCHEDULE_KEY.to_string(),
                    title: "Scheduled actions".to_string(),
                    tech: SCHEDULE_IDS.join(" + "),
                    description: "Lets the assistant set up recurring prompts for you — a \
                                  weekly summary, a monthly reminder — and list or remove \
                                  them. It always asks you to confirm before creating or \
                                  deleting one, and a schedule it creates runs without \
                                  tools. Manage them yourself on the Scheduled page."
                        .to_string(),
                    category: Category::Scheduling,
                });
            }
            continue;
        }
        if id.starts_with(TYPST_PREFIX) {
            // One row per template: the render id + its variants share a key.
            let key = entry_key_for(id);
            if typst_seen.insert(key.to_string()) {
                let (title, description) = templates
                    .iter()
                    .find(|m| m.key == key)
                    .map(|m| (m.title.clone(), short_description(&m.description)))
                    .unwrap_or_else(|| {
                        (
                            prettify(key.strip_prefix(TYPST_PREFIX).unwrap_or(key)),
                            "Fills this document template and returns a finished PDF and PNG \
                             to download."
                                .to_string(),
                        )
                    });
                out.push(ToolEntry {
                    key: key.to_string(),
                    title,
                    tech: key.to_string(),
                    description,
                    category: Category::Templates,
                });
            }
            continue;
        }
        if id.starts_with(COMFYUI_PREFIX) {
            // All comfyui_* ids collapse to a single "ComfyUI workflows"
            // toggle. The row's description lists the currently-loaded
            // workflows so the user sees what they get; a hot-reload that
            // adds a new workflow lands here automatically when the master
            // toggle is on (no per-workflow preference to chase).
            if comfyui_seen {
                continue;
            }
            comfyui_seen = true;
            let count = comfyui_metas
                .iter()
                .filter(|m| m.tool_id.starts_with(COMFYUI_PREFIX))
                .count();
            let names: Vec<String> = comfyui_metas
                .iter()
                .filter(|m| m.tool_id.starts_with(COMFYUI_PREFIX))
                .map(|m| m.title.clone())
                .collect();
            let description = if count == 0 {
                "Headless ComfyUI workflows (none loaded — visit /admin/comfyui to install). \
                 One switch enables or disables the whole family."
                    .to_string()
            } else {
                format!(
                    "Image / video / audio generation through the headless ComfyUI worker. \
                     Currently loaded: {}. One switch enables or disables the whole family.",
                    names.join(", ")
                )
            };
            out.push(ToolEntry {
                key: COMFYUI_KEY.to_string(),
                title: "ComfyUI workflows".to_string(),
                tech: format!("{COMFYUI_PREFIX}*"),
                description,
                category: Category::ComfyMedia,
            });
            continue;
        }
        // All of one MCP server's tools collapse to a single toggle, keyed
        // `mcp__<server>` — the integration is what the user reasons about,
        // and a server can expose a dozen+ tools. The key matches
        // `entry_key_for`, so the toggle actually governs every tool.
        if id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
            let key = entry_key_for(id);
            if mcp_servers_seen.insert(key.to_string()) {
                let server = key
                    .strip_prefix(crate::server::tools::mcp::MCP_ID_PREFIX)
                    .unwrap_or(key);
                out.push(ToolEntry {
                    key: key.to_string(),
                    title: format!("{server} (MCP)"),
                    tech: format!("{key}__*"),
                    description: format!(
                        "Tools bridged from the \"{server}\" MCP server (Model Context \
                         Protocol). One switch enables or disables the whole integration."
                    ),
                    category: Category::Integrations,
                });
            }
            continue;
        }
        let Some(tool) = registry.get(id) else {
            continue;
        };
        let def = tool.schema();
        let (title, description) = match display_meta(id) {
            Some((t, d)) => (t.to_string(), d.to_string()),
            None => (
                def.function.name.clone(),
                short_description(&def.function.description),
            ),
        };
        out.push(ToolEntry {
            key: id.clone(),
            title,
            tech: def.function.name,
            description,
            category: category_for(id),
        });
    }

    out.sort_by(|a, b| {
        a.category
            .order()
            .cmp(&b.category.order())
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// Drop every granted tool id whose toggle key the user disabled.
/// Honours the per-template typst collapse: disabling `typst_<id>` removes
/// that template's render + `_edit`/`_read`/`_pptx` ids at once.
pub fn retain_enabled(allowed: &mut Vec<String>, disabled_keys: &HashSet<String>) {
    allowed.retain(|id| !disabled_keys.contains(entry_key_for(id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tools::time::CurrentTimestamp;

    // NB: the tests that need the *concrete* tool set (rag / memory / document /
    // search_web grouping) live in `crates/gateway-tools/tests/catalog.rs` — those
    // tools are in the crate above this one. The ones here only need tool *ids*,
    // plus `CurrentTimestamp`, which stays in this crate.

    /// Every sandbox tool belongs in "Code & Sandbox". `convert_document` and
    /// `edit_presentation` used to be missing from the arm and fell through to
    /// `Utility` — the `DOCUMENT_IDS` doc comment even claimed they were
    /// handled here, which is how it went unnoticed.
    #[test]
    fn every_sandbox_tool_is_categorised_as_code() {
        for id in [
            "run_in_sandbox",
            "generate_document",
            "convert_document",
            "edit_presentation",
            "capture_webpage",
            "read_sandbox_output",
            "render_excalidraw",
            "render_typst",
        ] {
            assert_eq!(
                category_for(id),
                Category::Code,
                "`{id}` must be grouped with the sandbox tools, not fall through to Utility"
            );
        }
    }

    /// The sandbox tools that share the "document" toggle key are the canvas
    /// ones only — `export_document` is in `DOCUMENT_IDS`, but
    /// `generate_document` / `convert_document` must stay separate so enabling
    /// the canvas doesn't silently enable the sandbox.
    #[test]
    fn sandbox_document_tools_do_not_share_the_canvas_toggle() {
        assert_eq!(entry_key_for("export_document"), DOCUMENT_KEY);
        assert_eq!(entry_key_for("generate_document"), "generate_document");
        assert_eq!(entry_key_for("convert_document"), "convert_document");
    }

    #[test]
    fn sandbox_tools_have_hand_written_display_copy() {
        // Without this the /tools page shows the model-facing schema text.
        for id in ["convert_document", "edit_presentation"] {
            assert!(
                has_display_copy(id),
                "`{id}` needs plain-language display copy, not its LLM description"
            );
        }
    }

    /// `read_sandbox_output` resolves its `full_output_ref` against the
    /// *current turn's* attachments and hard-fails without one, so the `/v1`
    /// proxy paths must not advertise it — every call there would error.
    /// Every memory tool shares the one toggle *and* the one category. Both
    /// used to be spelled out as literals, so adding `update_memory` /
    /// `forget` left them in `Utility` and outside the switch.
    #[test]
    fn every_memory_tool_shares_the_toggle_and_the_category() {
        for id in MEMORY_IDS {
            assert_eq!(entry_key_for(id), MEMORY_KEY, "`{id}` toggle key");
            assert_eq!(category_for(id), Category::Memory, "`{id}` category");
        }
        // And the mutating pair is really in the list — a toggle that leaves
        // them on would let a user who disabled memory still have it written.
        assert!(MEMORY_IDS.contains(&"forget"));
        assert!(MEMORY_IDS.contains(&"update_memory"));
    }

    #[test]
    fn read_sandbox_output_is_chat_only() {
        assert!(requires_chat_session("read_sandbox_output"));
    }

    /// The bootstrap must not be presented as a toggle: the session-level
    /// allow-list force-keeps it, so the switch could never take effect, and
    /// it has no display copy so it rendered as LLM prose under "Utility".
    #[test]
    fn the_bootstrap_tool_is_never_a_toggle_row() {
        assert!(is_hidden(BOOTSTRAP_TOOL_ID));
        let reg = ToolRegistry::new().with(CurrentTimestamp);
        let rows = entries(
            &reg,
            &[
                BOOTSTRAP_TOOL_ID.to_string(),
                "get_current_timestamp".into(),
            ],
            &[],
            &[],
        );
        assert_eq!(
            rows.iter().filter(|e| e.key == BOOTSTRAP_TOOL_ID).count(),
            0,
            "enable_tools must not render a row: {rows:?}"
        );
        assert_eq!(rows.len(), 1, "the other tool still renders: {rows:?}");
    }

    /// The counterpart: tools that merely degrade off-chat must stay
    /// advertised there. `render_typst` needs a session only when the call
    /// references a canvas document; `run_in_sandbox` returns a URL instead of
    /// an attachment ref. Marking them chat-only would remove working
    /// capability from every API caller.
    #[test]
    fn tools_that_only_degrade_off_chat_stay_advertised() {
        for id in [
            "render_typst",
            "render_excalidraw",
            "run_in_sandbox",
            "generate_document",
            "convert_document",
            "capture_webpage",
            "search_web",
            "fetch_url",
            "fetch_attachment",
        ] {
            assert!(
                !requires_chat_session(id),
                "`{id}` works off the chat path and must stay advertised on /v1"
            );
        }
    }
}
