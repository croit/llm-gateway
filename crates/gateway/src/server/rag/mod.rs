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
//!   * [`index`] — usearch wrapper, one file per collection.
//!   * [`worker`] — the background task that ties the above together
//!     against the `rag_collections` table.
//!
//! See `docs/rag.md` (added later) for the operator-facing story; this
//! file is the entry point for code wanting to reach into the indexer.

pub mod chunk;
pub mod git;
pub mod index;
pub mod walk;
pub mod worker;
