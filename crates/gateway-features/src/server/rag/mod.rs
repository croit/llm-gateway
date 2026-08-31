// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG subsystem — the indexer side.
//!
//! Submodules:
//!   * [`chunk`] — sliding-window chunker (pure, content → chunks).
//!   * [`walk`] — filesystem walker + simple glob matcher used to decide
//!     which files in a cloned repo we feed to the chunker.
//!   * [`git`] — git clone/fetch helpers; shell out to system `git` so we
//!     don't pull `gix`/`git2` for one feature.
//!   * [`extract`] — bytes → text: the tier ladder (text, PDF text layer,
//!     OCR, office) that keeps documents out of the silent-skip path.
//!   * [`rerank`] — optional cross-encoder second opinion over the fused
//!     candidates, for corpora where every document looks alike.
//!   * [`sync`] — what actually changed since the last pass, so a re-index
//!     costs the delta rather than the corpus.
//!   * [`source`] — pluggable remote file providers (WebDAV today;
//!     OneDrive/Graph, Dropbox and S3 slot in beside it) plus the
//!     provider-agnostic tree walker.
//!   * [`index`] — usearch wrapper, one file per collection.
//!   * [`profile`] — the per-document extraction pass: normalised fields +
//!     a summary, which is what makes questions about *sets* of documents
//!     answerable rather than only questions about passages.
//!   * [`worker`] — the background task that ties the above together
//!     against the `rag_collections` table.
//!
//! See `docs/rag.md` (added later) for the operator-facing story; this
//! file is the entry point for code wanting to reach into the indexer.

/// Lowercase hex SHA-256. The content hash that decides whether a file
/// changed, and the key the profile-extraction cache is stored under.
pub(crate) fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub mod chunk;
pub mod extract;
pub mod git;
pub mod index;
pub mod profile;
pub mod rerank;
pub mod source;
pub mod sync;
pub mod walk;
pub mod worker;
