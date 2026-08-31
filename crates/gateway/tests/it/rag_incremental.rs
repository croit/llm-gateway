// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! A re-index costs the delta, not the corpus.
//!
//! The first build of a remote collection writes a fresh store; every build
//! after it updates that store in place. These tests assert the properties
//! that make that safe *and* worth doing, by counting what the source was
//! actually asked for:
//!
//!   * an unchanged corpus is not re-fetched,
//!   * a changed file is re-fetched and its old chunks go,
//!   * a moved file is a path update, not a re-fetch,
//!   * a deleted file's chunks and vectors go,
//!   * and a walk that failed part-way deletes nothing.
//!
//! The last one is the important one: a folder that returned 503 looks
//! exactly like a folder that was emptied, and treating it as the latter
//! silently removes a live subtree from the index.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::{self, rag as rag_db};
use gateway_core::server::upstreams::{
    UpstreamRegistry,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_features::server::rag::worker::{Indexer, IndexerConfig};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DAV_ROOT: &str = "/remote.php/dav/files/svc";

/// One file the fake server holds.
#[derive(Clone)]
struct File {
    name: &'static str,
    id: &'static str,
    etag: &'static str,
    body: &'static str,
}

fn multistatus(files: &[File], root_etag: &str) -> String {
    let mut out = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response><d:href>{DAV_ROOT}/</d:href><d:propstat><d:prop>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>&quot;{root_etag}&quot;</d:getetag><oc:fileid>1</oc:fileid>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#
    );
    for f in files {
        out.push_str(&format!(
            r#"
  <d:response><d:href>{DAV_ROOT}/{}</d:href><d:propstat><d:prop>
    <d:resourcetype/><d:getetag>&quot;{}&quot;</d:getetag><oc:fileid>{}</oc:fileid>
    <d:getcontentlength>{}</d:getcontentlength>
    <d:getcontenttype>text/plain</d:getcontenttype>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
            f.name,
            f.etag,
            f.id,
            f.body.len()
        ));
    }
    out.push_str("\n</d:multistatus>");
    out
}

/// A WebDAV server whose contents can be swapped between passes.
struct Dav {
    server: MockServer,
}

impl Dav {
    async fn start(files: &[File], root_etag: &str) -> Self {
        let server = MockServer::start().await;
        let dav = Self { server };
        dav.serve(files, root_etag).await;
        dav
    }

    /// Replace the mounted responses with a new state of the world.
    async fn serve(&self, files: &[File], root_etag: &str) {
        self.server.reset().await;
        Mock::given(method("PROPFIND"))
            .and(path(DAV_ROOT))
            .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(files, root_etag)))
            .mount(&self.server)
            .await;
        for f in files {
            Mock::given(method("GET"))
                .and(path(format!("{DAV_ROOT}/{}", f.name)))
                .respond_with(ResponseTemplate::new(200).set_body_string(f.body))
                .mount(&self.server)
                .await;
        }
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    /// How many file bodies were fetched — the number that says whether a
    /// pass did real work or merely confirmed nothing changed.
    async fn gets(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "GET")
            .count()
    }
}

async fn embedding_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let n = body
                .get("input")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let data: Vec<Value> = (0..n)
                .map(|i| json!({"object": "embedding", "index": i, "embedding": [1.0, 0.0]}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data}))
        })
        .mount(&server)
        .await;
    server
}

fn registry(embed: &str) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Embedding,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "e".into(),
                base_url: embed.into(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: vec!["embed-test".into()],
            }],
        },
    );
    let reg = UpstreamRegistry::new(&pools).unwrap();
    reg.pools()[0].backends[0].set_models(HashSet::from(["embed-test".to_string()]));
    reg
}

/// Everything one test needs, kept alive together.
struct Harness {
    indexer: Indexer,
    central: db::Pool,
    ref_id: i64,
    collection_id: i64,
    _embed: MockServer,
    _dir: TempDir,
}

impl Harness {
    async fn new(dav_uri: &str) -> Self {
        let embed = embedding_upstream().await;
        let reg = registry(&embed.uri());
        let central = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let crypto = Arc::new(Crypto::from_key([2u8; 32]));
        let indexer = Indexer::new(
            central.clone(),
            reg,
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: dir.path().to_path_buf(),
                ..IndexerConfig::default()
            },
            Some(Arc::clone(&crypto)),
        );
        let sealed = crypto
            .seal_str(&json!({"password": "pw"}).to_string())
            .unwrap();
        let c = rag_db::create_collection(
            &central,
            &rag_db::NewCollection {
                name: "docs".into(),
                description: None,
                git_url: String::new(),
                git_ref: "main".into(),
                pat: None,
                source: rag_db::SourceSpec {
                    kind: "webdav".into(),
                    config: [
                        ("base_url".to_string(), dav_uri.to_string()),
                        ("username".to_string(), "svc".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                    secrets: Some(sealed),
                },
                profile_id: None,
                extraction_model: None,
                embedding_model: "embed-test".into(),
                include_globs: Vec::new(),
                exclude_globs: Vec::new(),
                chunk_size: 400,
                chunk_overlap: 40,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        let r = rag_db::add_ref(&central, c.id, "main", None, true)
            .await
            .unwrap();
        Self {
            indexer,
            central,
            ref_id: r.id,
            collection_id: c.id,
            _embed: embed,
            _dir: dir,
        }
    }

    async fn index(&self) {
        // A ref left `ready` must be re-queued before the worker will build
        // it again, exactly as the "Re-index" button does.
        rag_db::request_reindex(&self.central, self.ref_id)
            .await
            .unwrap();
        self.indexer.index_ref(self.ref_id).await.unwrap();
        let after = rag_db::find_ref_by_id(&self.central, self.ref_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.status,
            rag_db::CollectionStatus::Ready,
            "last_error: {:?}",
            after.last_error
        );
    }

    /// Indexed paths, sorted.
    async fn paths(&self) -> Vec<String> {
        let r = rag_db::find_ref_by_id(&self.central, self.ref_id)
            .await
            .unwrap()
            .unwrap();
        let store = self
            .indexer
            .collection_store(r.id, &r.data_uuid)
            .await
            .unwrap();
        let mut paths: Vec<String> = rag_db::list_files_for_collection(&store, self.collection_id)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        paths.sort();
        paths
    }

    async fn chunk_count(&self) -> i64 {
        let r = rag_db::find_ref_by_id(&self.central, self.ref_id)
            .await
            .unwrap()
            .unwrap();
        let store = self
            .indexer
            .collection_store(r.id, &r.data_uuid)
            .await
            .unwrap();
        sqlx::query_scalar("SELECT COUNT(*) FROM rag_chunks")
            .fetch_one(&store)
            .await
            .unwrap()
    }

    async fn text_of(&self, path: &str) -> String {
        let r = rag_db::find_ref_by_id(&self.central, self.ref_id)
            .await
            .unwrap()
            .unwrap();
        let store = self
            .indexer
            .collection_store(r.id, &r.data_uuid)
            .await
            .unwrap();
        sqlx::query_scalar::<_, String>(
            "SELECT c.content FROM rag_chunks c JOIN rag_files f ON f.id = c.file_id \
             WHERE f.path = ? LIMIT 1",
        )
        .bind(path)
        .fetch_one(&store)
        .await
        .unwrap()
    }
}

fn file(name: &'static str, id: &'static str, etag: &'static str, body: &'static str) -> File {
    File {
        name,
        id,
        etag,
        body,
    }
}

#[tokio::test]
async fn an_unchanged_corpus_is_not_re_fetched() {
    let files = vec![
        file("a.txt", "10", "v1", "alpha content"),
        file("b.txt", "11", "v1", "beta content"),
    ];
    let dav = Dav::start(&files, "root1").await;
    let h = Harness::new(&dav.uri()).await;

    h.index().await;
    let after_first = dav.gets().await;
    assert_eq!(after_first, 2, "the first pass reads both files");

    // Nothing changed at the source.
    h.index().await;
    assert_eq!(
        dav.gets().await,
        after_first,
        "a second pass over an unchanged corpus must not re-download anything"
    );
    assert_eq!(h.paths().await, vec!["a.txt", "b.txt"]);
}

#[tokio::test]
async fn only_the_changed_file_is_re_fetched_and_its_old_chunks_go() {
    let dav = Dav::start(
        &[
            file("a.txt", "10", "v1", "alpha content"),
            file("b.txt", "11", "v1", "beta content"),
        ],
        "root1",
    )
    .await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;
    let before = dav.gets().await;
    let chunks_before = h.chunk_count().await;

    dav.serve(
        &[
            file("a.txt", "10", "v2", "alpha content, revised"),
            file("b.txt", "11", "v1", "beta content"),
        ],
        "root2",
    )
    .await;
    h.index().await;

    assert_eq!(
        dav.gets().await,
        1,
        "only the changed file was downloaded (the counter reset with the mounts)"
    );
    assert!(
        h.text_of("a.txt").await.contains("revised"),
        "the new content replaced the old"
    );
    assert_eq!(
        h.chunk_count().await,
        chunks_before,
        "the replaced file's old chunks were removed, not stacked on top"
    );
    let _ = before;
}

#[tokio::test]
async fn a_moved_file_is_a_path_update_not_a_re_fetch() {
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha content")], "root1").await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;

    // Same file id and etag, new name: the provider says this is the same
    // document. Re-extracting it would be pure waste — and on a corpus of
    // scans, hours of it.
    dav.serve(&[file("renamed.txt", "10", "v1", "alpha content")], "root2")
        .await;
    h.index().await;

    assert_eq!(h.paths().await, vec!["renamed.txt"]);
    assert_eq!(
        dav.gets().await,
        0,
        "a rename must not cost a download, let alone an extraction"
    );
}

#[tokio::test]
async fn a_deleted_file_leaves_the_index() {
    let dav = Dav::start(
        &[
            file("a.txt", "10", "v1", "alpha content"),
            file("b.txt", "11", "v1", "beta content"),
        ],
        "root1",
    )
    .await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;
    assert_eq!(h.chunk_count().await, 2);

    dav.serve(&[file("a.txt", "10", "v1", "alpha content")], "root2")
        .await;
    h.index().await;

    assert_eq!(h.paths().await, vec!["a.txt"]);
    assert_eq!(
        h.chunk_count().await,
        1,
        "the removed document's chunks went with it"
    );
}

#[tokio::test]
async fn a_failed_walk_deletes_nothing() {
    let dav = Dav::start(
        &[
            file("a.txt", "10", "v1", "alpha content"),
            file("b.txt", "11", "v1", "beta content"),
        ],
        "root1",
    )
    .await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;
    assert_eq!(h.paths().await.len(), 2);

    // The server breaks. An empty listing and a broken listing are the same
    // shape; only one of them means "these documents are gone".
    dav.server.reset().await;
    Mock::given(method("PROPFIND"))
        .and(path(DAV_ROOT))
        .respond_with(ResponseTemplate::new(503))
        .mount(&dav.server)
        .await;

    rag_db::request_reindex(&h.central, h.ref_id).await.unwrap();
    let _ = h.indexer.index_ref(h.ref_id).await;

    assert_eq!(
        h.paths().await.len(),
        2,
        "a source that could not be listed must never be read as an empty source"
    );
}

#[tokio::test]
async fn an_unchanged_subtree_is_pruned_on_the_second_pass() {
    // The root etag is unchanged, so the provider reports the whole tree
    // unchanged and the walker skips it. Nothing is fetched, and — the part
    // that would be a disaster to get wrong — nothing is deleted either.
    let files = vec![file("a.txt", "10", "v1", "alpha content")];
    let dav = Dav::start(&files, "root1").await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;
    let listings_before = dav
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "PROPFIND")
        .count();

    h.index().await;

    assert_eq!(
        h.paths().await,
        vec!["a.txt"],
        "the corpus survived pruning"
    );
    let listings_after = dav
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "PROPFIND")
        .count();
    assert!(
        listings_after > listings_before,
        "the root is still checked — pruning skips descending, not checking"
    );
    assert_eq!(dav.gets().await, 1, "and no file was re-read");
}

#[tokio::test]
async fn the_sync_hook_requeues_the_collection_it_belongs_to() {
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha")], "root1").await;
    let h = Harness::new(&dav.uri()).await;
    h.index().await;

    let token = rag_db::rotate_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    // The token is a credential, so only its hash is stored.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT sync_token_hash FROM rag_collections WHERE id = ?")
            .bind(h.collection_id)
            .fetch_one(&h.central)
            .await
            .unwrap();
    let stored = stored.expect("a hash was written");
    assert_ne!(stored, token, "the plaintext token must never be stored");
    assert_eq!(stored, rag_db::hash_sync_token(&token));

    // The lookup is by hash, and it finds the right collection.
    let found = rag_db::find_by_sync_token(&h.central, &token)
        .await
        .unwrap()
        .expect("the token resolves");
    assert_eq!(found.id, h.collection_id);
    assert!(found.sync_hook_set, "the row reports that a hook exists");

    // Firing it puts the ref back in the queue.
    for r in rag_db::list_refs(&h.central, h.collection_id)
        .await
        .unwrap()
    {
        h.indexer.request_reindex(r.id).await.unwrap();
        let after = rag_db::find_ref_by_id(&h.central, r.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.status, rag_db::CollectionStatus::Pending);
    }
}

#[tokio::test]
async fn a_wrong_sync_token_resolves_to_nothing() {
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha")], "root1").await;
    let h = Harness::new(&dav.uri()).await;
    rag_db::rotate_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    assert!(
        rag_db::find_by_sync_token(&h.central, "not-the-token")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn rotating_invalidates_the_previous_sync_url() {
    // The point of rotating: the old URL must stop working.
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha")], "root1").await;
    let h = Harness::new(&dav.uri()).await;
    let first = rag_db::rotate_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    let second = rag_db::rotate_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    assert_ne!(first, second);
    assert!(
        rag_db::find_by_sync_token(&h.central, &first)
            .await
            .unwrap()
            .is_none(),
        "the rotated-away token no longer works"
    );
    assert!(
        rag_db::find_by_sync_token(&h.central, &second)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn clearing_the_token_disables_the_hook() {
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha")], "root1").await;
    let h = Harness::new(&dav.uri()).await;
    let token = rag_db::rotate_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    rag_db::clear_sync_token(&h.central, h.collection_id)
        .await
        .unwrap();
    assert!(
        rag_db::find_by_sync_token(&h.central, &token)
            .await
            .unwrap()
            .is_none()
    );
    let c = rag_db::find_collection_by_id(&h.central, h.collection_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!c.sync_hook_set);
}

/// Guards against a regression where the *first* build of a ref took the
/// incremental path and tried to diff against a store that does not exist.
#[tokio::test]
async fn the_first_build_still_writes_a_fresh_store() {
    static SEEN: AtomicBool = AtomicBool::new(false);
    SEEN.store(false, Ordering::Relaxed);
    let dav = Dav::start(&[file("a.txt", "10", "v1", "alpha")], "root1").await;
    let h = Harness::new(&dav.uri()).await;

    let before = rag_db::find_ref_by_id(&h.central, h.ref_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!before.is_searchable(), "nothing indexed yet");
    h.index().await;
    let after = rag_db::find_ref_by_id(&h.central, h.ref_id)
        .await
        .unwrap()
        .unwrap();
    assert!(after.is_searchable());
    assert_ne!(
        after.data_uuid, before.data_uuid,
        "the first build swapped onto a fresh folder"
    );
    assert_eq!(h.paths().await, vec!["a.txt"]);
}
