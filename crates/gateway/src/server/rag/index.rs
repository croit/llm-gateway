// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-collection usearch index wrapper.
//!
//! One on-disk file per collection at `<data_dir>/rag/<collection_id>.usearch`
//! (operator chooses `<data_dir>` via `[rag] data_dir = …`, defaults to
//! `data/rag` alongside the sqlite file). Cosine distance, F32 vectors —
//! the embedding upstreams we route to (BGE family, OpenAI's
//! `text-embedding-3-small`, Voyage, etc.) all emit F32 vectors meant to
//! be normalised + compared with cosine, so we don't need usearch's
//! quantization knobs for V1.
//!
//! Vector ids are `i64` here (matching the SQLite `INTEGER PRIMARY KEY`
//! shape) but usearch uses `u64`. The indexer hands out monotonic ids
//! starting at 1, so the cast is lossless in both directions.
//!
//! Thread-safety: usearch's `Index` is `Send + Sync` and uses internal
//! locking on the C++ side, but we still wrap it in a `Mutex` so the
//! Rust-side state (the file path, the dimensions hint we carry around)
//! moves with it and so the worker + search-tool paths can't race the
//! save call against an in-flight add.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use thiserror::Error;
use usearch::ffi::{IndexOptions, MetricKind, ScalarKind};
use usearch::{Index, new_index};

/// Capture a usearch (cxx) error as its rendered message so callers
/// don't need a `cxx` dep just to format the chain. Used everywhere we
/// take a `Result<_, cxx::Exception>` from the upstream crate.
fn native_msg<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("creating directory for index `{path}`: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("opening usearch index at `{path}`: {message}")]
    Open { path: PathBuf, message: String },
    #[error(
        "index `{path}` was built for {found}-dim vectors but the collection's embedding model emits {expected}-dim vectors — recreate the index"
    )]
    DimensionMismatch {
        path: PathBuf,
        expected: usize,
        found: usize,
    },
    #[error("usearch error: {0}")]
    Native(String),
    #[error("vector length {got} does not match index dimensions {expected}")]
    BadVectorLen { expected: usize, got: usize },
}

/// One open usearch index, with the metadata the wrapper needs to gate
/// inserts and surface helpful errors. The inner `Index` itself isn't
/// `Debug` (cxx-owned), so we hand-roll a terse impl that's good enough
/// for `dbg!` / panic messages without recursing into the C++ side.
pub struct CollectionIndex {
    inner: Mutex<Index>,
    path: PathBuf,
    dimensions: usize,
}

impl std::fmt::Debug for CollectionIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionIndex")
            .field("path", &self.path)
            .field("dimensions", &self.dimensions)
            .finish()
    }
}

impl CollectionIndex {
    /// Open the index at `path`, or create a fresh one for `dimensions`-
    /// dim vectors when no file exists yet.
    pub fn open_or_create(path: &Path, dimensions: usize) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IndexError::Mkdir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        if path.exists() {
            Self::restore_existing(path, dimensions)
        } else {
            Self::create_new(path, dimensions)
        }
    }

    fn restore_existing(path: &Path, expected: usize) -> Result<Self, IndexError> {
        let make_open_err = || IndexError::Open {
            path: path.to_path_buf(),
            message: String::new(),
        };
        let meta = Index::metadata(&path.to_string_lossy()).map_err(|e| {
            let mut err = make_open_err();
            if let IndexError::Open { message, .. } = &mut err {
                *message = native_msg(e);
            }
            err
        })?;
        let found = meta.dimensions as usize;
        if found != expected {
            return Err(IndexError::DimensionMismatch {
                path: path.to_path_buf(),
                expected,
                found,
            });
        }
        let opts = build_options(expected);
        let index = new_index(&opts).map_err(|e| IndexError::Open {
            path: path.to_path_buf(),
            message: native_msg(e),
        })?;
        index
            .load(&path.to_string_lossy())
            .map_err(|e| IndexError::Open {
                path: path.to_path_buf(),
                message: native_msg(e),
            })?;
        Ok(Self {
            inner: Mutex::new(index),
            path: path.to_path_buf(),
            dimensions: expected,
        })
    }

    fn create_new(path: &Path, dimensions: usize) -> Result<Self, IndexError> {
        let opts = build_options(dimensions);
        let index = new_index(&opts).map_err(|e| IndexError::Open {
            path: path.to_path_buf(),
            message: native_msg(e),
        })?;
        index
            .save(&path.to_string_lossy())
            .map_err(|e| IndexError::Open {
                path: path.to_path_buf(),
                message: native_msg(e),
            })?;
        Ok(Self {
            inner: Mutex::new(index),
            path: path.to_path_buf(),
            dimensions,
        })
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("usearch mutex poisoned").size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert a single vector. Caller is responsible for monotonic id
    /// allocation (see `db::rag::max_vector_id`).
    pub fn add(&self, vector_id: i64, vector: &[f32]) -> Result<(), IndexError> {
        if vector.len() != self.dimensions {
            return Err(IndexError::BadVectorLen {
                expected: self.dimensions,
                got: vector.len(),
            });
        }
        let guard = self.inner.lock().expect("usearch mutex poisoned");
        // usearch's HNSW capacity is auto-grown by `add`; we reserve in
        // small steps when needed so a re-index of a huge file doesn't
        // round-trip the allocator on every insert.
        let cap = guard.capacity();
        let size = guard.size();
        if size + 1 > cap {
            guard
                .reserve(cap.max(64) * 2)
                .map_err(|e| IndexError::Native(native_msg(e)))?;
        }
        guard
            .add(vector_id as u64, vector)
            .map_err(|e| IndexError::Native(native_msg(e)))?;
        Ok(())
    }

    /// Remove a single vector. Idempotent: removing a key that isn't
    /// present is *not* an error (returns `Ok(false)`).
    pub fn remove(&self, vector_id: i64) -> Result<bool, IndexError> {
        let guard = self.inner.lock().expect("usearch mutex poisoned");
        let removed = guard
            .remove(vector_id as u64)
            .map_err(|e| IndexError::Native(native_msg(e)))?;
        Ok(removed > 0)
    }

    /// k-NN search. Returns `(vector_id, distance)` pairs. usearch uses
    /// cosine *distance* with the `Cos` metric (smaller = closer); the
    /// search-tool layer converts to similarity when it renders to the
    /// model.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(i64, f32)>, IndexError> {
        if query.len() != self.dimensions {
            return Err(IndexError::BadVectorLen {
                expected: self.dimensions,
                got: query.len(),
            });
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let guard = self.inner.lock().expect("usearch mutex poisoned");
        let matches = guard
            .search(query, k)
            .map_err(|e| IndexError::Native(native_msg(e)))?;
        Ok(matches
            .keys
            .iter()
            .zip(matches.distances.iter())
            .map(|(k, d)| (*k as i64, *d))
            .collect())
    }

    /// Flush the index to disk at its configured path. The indexer calls
    /// this after a batch of inserts so a crash mid-run never costs more
    /// than the in-flight batch.
    pub fn save(&self) -> Result<(), IndexError> {
        let guard = self.inner.lock().expect("usearch mutex poisoned");
        guard
            .save(&self.path.to_string_lossy())
            .map_err(|e| IndexError::Open {
                path: self.path.clone(),
                message: native_msg(e),
            })?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn build_options(dimensions: usize) -> IndexOptions {
    IndexOptions {
        dimensions,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn vec_of(seed: f32, dims: usize) -> Vec<f32> {
        (0..dims).map(|i| seed + i as f32 * 0.01).collect()
    }

    #[test]
    fn create_add_search_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.usearch");
        let index = CollectionIndex::open_or_create(&path, 4).unwrap();
        index.add(1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        index.add(2, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        index.add(3, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        let hits = index.search(&[1.0, 0.0, 0.0, 0.0], 2).unwrap();
        let keys: Vec<i64> = hits.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&1) && keys.contains(&3), "got {keys:?}");
    }

    #[test]
    fn reopen_existing_preserves_vectors_and_rejects_bad_dimensions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.usearch");
        {
            let index = CollectionIndex::open_or_create(&path, 4).unwrap();
            index.add(1, &vec_of(1.0, 4)).unwrap();
            index.save().unwrap();
        }
        // Round-trip the same dims.
        {
            let index = CollectionIndex::open_or_create(&path, 4).unwrap();
            assert_eq!(index.len(), 1);
            let hits = index.search(&vec_of(1.0, 4), 1).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, 1);
        }
        // Trying to reopen with a different dim must fail loudly.
        let err = CollectionIndex::open_or_create(&path, 8).unwrap_err();
        assert!(
            matches!(
                err,
                IndexError::DimensionMismatch {
                    expected: 8,
                    found: 4,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn remove_drops_key_from_subsequent_search() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.usearch");
        let index = CollectionIndex::open_or_create(&path, 4).unwrap();
        index.add(7, &vec_of(0.5, 4)).unwrap();
        index.add(8, &vec_of(0.5, 4)).unwrap();
        assert!(index.remove(7).unwrap());
        // Removing again is a no-op, not an error.
        assert!(!index.remove(7).unwrap());
        let hits = index.search(&vec_of(0.5, 4), 5).unwrap();
        let keys: Vec<i64> = hits.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&7));
    }

    #[test]
    fn wrong_vector_length_is_a_clear_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.usearch");
        let index = CollectionIndex::open_or_create(&path, 4).unwrap();
        let err = index.add(1, &[1.0, 0.0]).unwrap_err();
        assert!(
            matches!(
                err,
                IndexError::BadVectorLen {
                    expected: 4,
                    got: 2
                }
            ),
            "{err:?}"
        );
        let err = index.search(&[1.0, 0.0], 5).unwrap_err();
        assert!(
            matches!(
                err,
                IndexError::BadVectorLen {
                    expected: 4,
                    got: 2
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn search_with_k_zero_returns_empty_without_touching_the_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("idx.usearch");
        let index = CollectionIndex::open_or_create(&path, 4).unwrap();
        let hits = index.search(&vec_of(0.0, 4), 0).unwrap();
        assert!(hits.is_empty());
    }
}
