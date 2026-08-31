// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH
//
// LIVE, no-mock E2E against a real Nextcloud. Gated behind
// RUN_NEXTCLOUD_E2E so it never runs in normal CI or a plain
// `cargo test` — it needs a container.
//
// Run (starts and tears down the container for you):
//   mise run test-nextcloud
//
// Or against an existing server:
//   RUN_NEXTCLOUD_E2E=1 NEXTCLOUD_URL=http://127.0.0.1:8099 \
//   NEXTCLOUD_USER=admin NEXTCLOUD_PASS=admin-password \
//   cargo test -p gateway --test nextcloud_e2e_live -- --nocapture --test-threads=1
//
// ## Why this exists
//
// The mocked tests pin our *model* of WebDAV. This one pins the parts of
// that model that are actually assumptions about a real server, and which
// would silently produce a broken or wildly expensive index if wrong:
//
//   * `oc:fileid` is really returned, so file identity survives a move —
//     without it, reorganising a folder re-OCRs every document in it.
//   * A collection's etag really changes when something beneath it changes,
//     and really propagates upward. Subtree pruning is built on this; if it
//     does not hold, a cheap re-sync silently misses changes.
//   * The default DAV path template and Basic auth are right.
//   * Percent-encoded names (spaces, umlauts) round-trip through PROPFIND
//     hrefs and back out as fetchable URLs.
//
// Each test drives the real `WebdavProvider` — no mock server anywhere.

use std::collections::BTreeMap;

use gateway_features::server::rag::source::{
    DirListing, DirRef, EntryKind, FileProvider, ProviderConfig, ProviderRegistry,
};

fn enabled() -> bool {
    std::env::var("RUN_NEXTCLOUD_E2E").is_ok()
}

fn base_url() -> String {
    std::env::var("NEXTCLOUD_URL").unwrap_or_else(|_| "http://127.0.0.1:8099".into())
}

fn user() -> String {
    std::env::var("NEXTCLOUD_USER").unwrap_or_else(|_| "admin".into())
}

fn pass() -> String {
    std::env::var("NEXTCLOUD_PASS").unwrap_or_else(|_| "admin-password".into())
}

/// A provider rooted at `root`, built the way the admin form builds one.
fn provider(root: &str) -> std::sync::Arc<dyn FileProvider> {
    let values: BTreeMap<String, String> = [
        ("base_url".to_string(), base_url()),
        ("username".to_string(), user()),
        ("root".to_string(), root.to_string()),
    ]
    .into_iter()
    .collect();
    let secrets: BTreeMap<String, String> =
        [("password".to_string(), pass())].into_iter().collect();
    ProviderRegistry::with_builtins()
        .build(
            "webdav",
            &ProviderConfig::new(values, secrets),
            reqwest::Client::new(),
        )
        .expect("the webdav provider builds from a valid config")
}

fn dav_url(path: &str) -> String {
    format!(
        "{}/remote.php/dav/files/{}/{}",
        base_url().trim_end_matches('/'),
        user(),
        path.trim_start_matches('/')
    )
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn mkcol(path: &str) {
    let resp = http()
        .request(
            reqwest::Method::from_bytes(b"MKCOL").unwrap(),
            dav_url(path),
        )
        .basic_auth(user(), Some(pass()))
        .send()
        .await
        .expect("MKCOL reaches the server");
    // 405 = already there, which is fine for a re-run.
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 405,
        "MKCOL {path} failed: {}",
        resp.status()
    );
}

async fn put(path: &str, body: &str) {
    let resp = http()
        .put(dav_url(path))
        .basic_auth(user(), Some(pass()))
        .body(body.to_string())
        .send()
        .await
        .expect("PUT reaches the server");
    assert!(
        resp.status().is_success(),
        "PUT {path} failed: {}",
        resp.status()
    );
}

async fn move_to(from: &str, to: &str) {
    let resp = http()
        .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), dav_url(from))
        .basic_auth(user(), Some(pass()))
        .header("Destination", dav_url(to))
        .header("Overwrite", "T")
        .send()
        .await
        .expect("MOVE reaches the server");
    assert!(
        resp.status().is_success(),
        "MOVE {from} -> {to} failed: {}",
        resp.status()
    );
}

async fn delete(path: &str) {
    let _ = http()
        .delete(dav_url(path))
        .basic_auth(user(), Some(pass()))
        .send()
        .await;
}

/// List a provider's root, failing the test with the server's own message
/// rather than a bare unwrap.
async fn list_root(p: &std::sync::Arc<dyn FileProvider>) -> DirListing {
    p.list_dir(&p.root())
        .await
        .unwrap_or_else(|e| panic!("listing the root failed: {e}"))
}

fn entries(listing: &DirListing) -> Vec<gateway_features::server::rag::source::RemoteEntry> {
    match listing {
        DirListing::Listed { entries, .. } => entries.clone(),
        DirListing::Unchanged => panic!("expected a listing, got Unchanged"),
    }
}

fn version_of(listing: &DirListing) -> Option<String> {
    match listing {
        DirListing::Listed { version, .. } => version.clone(),
        DirListing::Unchanged => None,
    }
}

/// Give each test its own folder so they cannot interfere, and re-runs start
/// clean.
async fn fresh_folder(name: &str) -> String {
    let folder = format!("gw-e2e-{name}");
    delete(&folder).await;
    mkcol(&folder).await;
    folder
}

#[tokio::test]
async fn the_provider_reaches_a_real_server_and_reports_its_extensions() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1 (or run `mise run test-nextcloud`)");
        return;
    }
    let folder = fresh_folder("probe").await;
    put(&format!("{folder}/hello.txt"), "hello from the e2e test").await;

    let p = provider(&folder);
    let report = p
        .probe()
        .await
        .unwrap_or_else(|e| panic!("probe failed: {e}"));
    assert_eq!(report.account.as_deref(), Some(user().as_str()));
    assert_eq!(report.root_entries, 1, "the file we just wrote is listed");

    // The capability detection that everything else keys off: a real
    // Nextcloud must present the ownCloud extension properties.
    let caps = p.capabilities();
    assert!(
        caps.stable_ids,
        "oc:fileid was not detected — file identity would fall back to paths, \
         and every folder reorganisation would re-extract its whole contents"
    );
    assert!(
        caps.subtree_pruning,
        "propagating collection etags were not detected — every re-sync would \
         walk the entire tree"
    );
    // The link back to the original, checked directly rather than through a
    // capability flag: this is what a citation in an answer resolves to.
    let listing = p
        .list_dir(&p.root())
        .await
        .unwrap_or_else(|e| panic!("listing the root failed: {e}"));
    let DirListing::Listed { entries, .. } = listing else {
        panic!("a cold listing is never Unchanged");
    };
    let file = entries
        .iter()
        .find(|e| e.rel_path.ends_with("hello.txt"))
        .expect("the file we wrote is in the listing");
    let link = p
        .web_url(file)
        .expect("a real Nextcloud yields a browser link for an indexed file");
    assert!(
        link.contains(&file.id),
        "the link addresses the file by its stable id, so it survives a move: {link}"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn a_real_listing_carries_fileid_etag_size_and_type() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    let folder = fresh_folder("listing").await;
    put(&format!("{folder}/invoice.txt"), "total 1234.56 EUR").await;
    mkcol(&format!("{folder}/sub")).await;

    let p = provider(&folder);
    let listing = list_root(&p).await;
    let items = entries(&listing);

    let file = items
        .iter()
        .find(|e| e.rel_path == "invoice.txt")
        .expect("the file is listed");
    assert_eq!(file.kind, EntryKind::File);
    assert!(
        file.id.parse::<u64>().is_ok(),
        "oc:fileid should be numeric, got {:?}",
        file.id
    );
    assert!(!file.version.is_empty(), "an etag came back");
    assert_eq!(
        file.size_bytes, 17,
        "getcontentlength matched the bytes we wrote"
    );
    assert_eq!(file.mime.as_deref(), Some("text/plain"));
    assert!(file.modified_at.is_some(), "getlastmodified parsed");

    let dir = items
        .iter()
        .find(|e| e.rel_path == "sub")
        .expect("subfolder listed");
    assert_eq!(
        dir.kind,
        EntryKind::Dir,
        "resourcetype distinguished a collection"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn a_folder_etag_changes_when_something_beneath_it_changes() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    // THE load-bearing assumption. Subtree pruning skips a directory whose
    // etag matches the stored one; if the etag did not move on a nested
    // change, a cheap re-sync would silently miss it.
    let folder = fresh_folder("etag").await;
    mkcol(&format!("{folder}/deep")).await;
    put(&format!("{folder}/deep/a.txt"), "one").await;

    let p = provider(&folder);
    let before = version_of(&list_root(&p).await).expect("the root reports an etag");

    // Change a file two levels down.
    put(&format!("{folder}/deep/a.txt"), "two — changed").await;

    let after = version_of(&list_root(&p).await).expect("the root still reports an etag");
    assert_ne!(
        before, after,
        "the root etag did not change after a nested edit — subtree pruning \
         would skip a subtree that actually changed"
    );

    // And the converse: with nothing touched, the provider answers Unchanged
    // for a directory whose stored version still matches.
    let dir = DirRef {
        locator: String::new(),
        rel_path: String::new(),
        known_version: Some(after.clone()),
    };
    let again = p.list_dir(&dir).await.expect("re-list succeeds");
    assert!(
        matches!(again, DirListing::Unchanged),
        "an unchanged tree should prune, got a full listing"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn a_file_id_survives_a_move() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    // The other load-bearing assumption: identity is the file id, so a
    // reorganised folder is a path update rather than a re-extraction.
    let folder = fresh_folder("move").await;
    put(&format!("{folder}/before.txt"), "same bytes").await;

    let p = provider(&folder);
    let before = entries(&list_root(&p).await)
        .into_iter()
        .find(|e| e.rel_path == "before.txt")
        .expect("listed before the move");

    mkcol(&format!("{folder}/moved")).await;
    move_to(
        &format!("{folder}/before.txt"),
        &format!("{folder}/moved/after.txt"),
    )
    .await;

    let sub = DirRef {
        locator: "moved".into(),
        rel_path: "moved".into(),
        known_version: None,
    };
    let after = entries(&p.list_dir(&sub).await.expect("listing the subfolder"))
        .into_iter()
        .find(|e| e.rel_path.ends_with("after.txt"))
        .expect("listed after the move");

    assert_eq!(
        before.id, after.id,
        "the file id changed across a move — every reorganisation would look \
         like a delete plus a fresh document, and re-run OCR on all of it"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn names_with_spaces_and_umlauts_round_trip() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    // The href comes back percent-encoded and has to decode to the real name,
    // then re-encode into a URL we can actually fetch. A German document
    // archive is full of these.
    let folder = fresh_folder("encoding").await;
    let name = "Rechnung Müller & Co (2025).txt";
    let body = "Rechnung über 1.234,56 €";
    put(&format!("{folder}/{name}"), body).await;

    let p = provider(&folder);
    let entry = entries(&list_root(&p).await)
        .into_iter()
        .find(|e| e.rel_path == name)
        .unwrap_or_else(|| panic!("`{name}` did not decode back to its real name"));

    let bytes = p
        .fetch(&entry, 1_000_000)
        .await
        .unwrap_or_else(|e| panic!("fetching `{name}` failed: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        body,
        "the re-encoded fetch URL resolved to the right file"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn the_walker_recurses_a_real_tree_and_prunes_on_the_second_pass() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    use gateway_features::server::rag::source::tree;
    use gateway_features::server::rag::walk::Filter;

    let folder = fresh_folder("walk").await;
    mkcol(&format!("{folder}/a")).await;
    mkcol(&format!("{folder}/a/b")).await;
    put(&format!("{folder}/top.txt"), "top").await;
    put(&format!("{folder}/a/mid.txt"), "mid").await;
    put(&format!("{folder}/a/b/deep.txt"), "deep").await;

    let p = provider(&folder);
    let filter = Filter::new(&[], &[], u64::MAX);
    let opts = tree::WalkOptions::default();

    let first = tree::walk(p.clone(), &tree::DirVersions::new(), &filter, &opts)
        .await
        .expect("the first walk succeeds");
    let mut paths: Vec<&str> = first.files.iter().map(|f| f.rel_path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["a/b/deep.txt", "a/mid.txt", "top.txt"]);
    assert!(first.is_complete(), "no directory failed to list");

    // Second pass with the versions the first recorded: nothing changed, so
    // every directory should prune.
    let second = tree::walk(p.clone(), &first.dir_versions, &filter, &opts)
        .await
        .expect("the second walk succeeds");
    assert!(
        second.files.is_empty(),
        "an unchanged tree still produced files to index: {:?}",
        second.files.iter().map(|f| &f.rel_path).collect::<Vec<_>>()
    );
    assert!(
        !second.pruned.is_empty(),
        "nothing was pruned — a nightly re-sync would re-walk the whole corpus"
    );
    delete(&folder).await;
}

#[tokio::test]
async fn a_wrong_password_is_reported_as_a_credential_problem() {
    if !enabled() {
        eprintln!("skipping: set RUN_NEXTCLOUD_E2E=1");
        return;
    }
    let values: BTreeMap<String, String> = [
        ("base_url".to_string(), base_url()),
        ("username".to_string(), user()),
    ]
    .into_iter()
    .collect();
    let secrets: BTreeMap<String, String> = [("password".to_string(), "definitely-wrong".into())]
        .into_iter()
        .collect();
    let p = ProviderRegistry::with_builtins()
        .build(
            "webdav",
            &ProviderConfig::new(values, secrets),
            reqwest::Client::new(),
        )
        .expect("builds");

    let err = p
        .probe()
        .await
        .expect_err("a wrong password must not succeed");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("credential"),
        "the operator should be told to fix the credentials, got: {err}"
    );
}
