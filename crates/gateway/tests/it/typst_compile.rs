// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! End-to-end typst compile test against the bundled example
//! template. Requires the `typst` CLI on PATH; gated on
//! `typst --version` succeeding so machines without typst skip
//! gracefully instead of failing the suite.
//!
//! What we're pinning here is the whole compile-and-handoff path:
//! discovery picks up the example, `typst::compile` produces real
//! PDF + PNG bytes, and the bytes look right (PDF magic bytes,
//! PNG signature, source file round-tripped).

use gateway_features::server::typst as gw_typst;

/// Skip the test body when typst isn't installed; lets CI machines
/// without typst run the rest of the suite.
fn typst_available() -> bool {
    std::process::Command::new("typst")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn example_dir() -> std::path::PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // crates/gateway → workspace root → examples
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("examples/typst-templates")
}

#[tokio::test]
async fn compiles_example_letter_template() {
    if !typst_available() {
        eprintln!("skipping: typst CLI not on PATH");
        return;
    }
    let templates = gw_typst::discover_templates(&example_dir()).expect("discover");
    let letter = templates
        .iter()
        .find(|t| t.id == "letter")
        .expect("letter template not found under examples/typst-templates");
    let inputs = vec![
        ("recipient_name".to_string(), "Acme Corp".to_string()),
        (
            "recipient_address".to_string(),
            "123 Main St\nSpringfield, USA".to_string(),
        ),
        ("subject".to_string(), "Integration test".to_string()),
        (
            "body".to_string(),
            "This letter is produced by the typst_compile integration test.".to_string(),
        ),
        ("sender_name".to_string(), "Jane Doe".to_string()),
    ];
    let rendered = gw_typst::compile(letter, &inputs, 1, None)
        .await
        .expect("compile");
    // PDF magic: "%PDF-"
    assert!(
        rendered.pdf.starts_with(b"%PDF-"),
        "PDF magic bytes missing"
    );
    assert!(rendered.pdf.len() > 1024, "pdf suspiciously small");
    // The letter sets `font: "Urbanist"`, shipped in the template's
    // `fonts/` subdir. `compile` must hand typst `--font-path` so the
    // brand typeface is found and embedded — otherwise typst warns
    // "unknown font family" and silently falls back. An embedded subset
    // carries the family name in its font descriptor, so the bytes of a
    // correctly-rendered PDF contain "Urbanist"; a fallback render does
    // not. This pins the --font-path wiring against regressions.
    assert!(
        rendered
            .pdf
            .windows(b"Urbanist".len())
            .any(|w| w == b"Urbanist"),
        "Urbanist not embedded — --font-path likely not passed to typst"
    );
    // PNG signature: 89 50 4E 47 0D 0A 1A 0A
    assert_eq!(
        &rendered.png[..8],
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        "PNG signature missing"
    );
    // Source round-trip — the bytes we get back must be the same
    // template.typ on disk, so the model gets a faithful copy to
    // iterate on.
    let on_disk = std::fs::read(letter.root.join(&letter.source_file)).unwrap();
    assert_eq!(rendered.source, on_disk);
}

/// The presentation example is discoverable and renders through the gateway
/// path (`--input deck=<inline JSON>`): its deck field carries the whole deck
/// as a JSON string, exercising the inline branch of the template's input
/// handling and the shipped neutral assets (logo, icons, fonts, gradient
/// backgrounds). Also confirms an escaped `@` in an eval'd prose field renders.
#[tokio::test]
async fn compiles_example_presentation_template() {
    if !typst_available() {
        eprintln!("skipping: typst CLI not on PATH");
        return;
    }
    let templates = gw_typst::discover_templates(&example_dir()).expect("discover");
    let deck = templates
        .iter()
        .find(|t| t.id == "presentation")
        .expect("presentation template not found under examples/typst-templates");
    // Minimal inline deck: a cover + a content slide whose body has an escaped
    // email (`\@`) in the eval'd markup, plus a card with a built-in icon.
    let deck_json = r#"{
        "deck_title": "Example Corp",
        "theme": "dark",
        "slides": [
            {"layout": "cover", "title": "Hello", "subtitle": "A sample deck"},
            {"layout": "content", "title": "Contact",
             "body": "Reach us at hi\\@example.com for *details*."},
            {"layout": "cards", "title": "Why us",
             "cards": [{"title": "Fast", "body": "Quick.", "icon": "zap"}]}
        ]
    }"#;
    let inputs = vec![("deck".to_string(), deck_json.to_string())];
    let rendered = gw_typst::compile(deck, &inputs, 1, None)
        .await
        .expect("presentation compile");
    assert!(rendered.pdf.starts_with(b"%PDF-"), "PDF magic missing");
    assert!(rendered.pdf.len() > 1024, "pdf suspiciously small");
    assert_eq!(
        &rendered.png[..8],
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        "PNG signature missing"
    );
}

#[tokio::test]
async fn compile_surfaces_typst_error_on_bad_input() {
    if !typst_available() {
        eprintln!("skipping: typst CLI not on PATH");
        return;
    }
    let templates = gw_typst::discover_templates(&example_dir()).expect("discover");
    let letter = templates.iter().find(|t| t.id == "letter").unwrap();
    // Omit required `recipient_name`. typst will fail when the
    // template tries to read it from `sys.inputs`; the error must
    // come back as `CompileError::Failed` carrying the stderr so
    // the model can see what's wrong and try again.
    let inputs = vec![
        ("subject".to_string(), "x".to_string()),
        ("body".to_string(), "x".to_string()),
        ("sender_name".to_string(), "x".to_string()),
        ("recipient_address".to_string(), "x".to_string()),
    ];
    let err = gw_typst::compile(letter, &inputs, 1, None)
        .await
        .unwrap_err();
    match err {
        gw_typst::CompileError::Failed(msg) => {
            assert!(
                msg.contains("recipient_name") || msg.contains("error"),
                "unexpected stderr: {msg}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Pins the premise behind the gateway's auto-escape-on-`@` retry (see
/// `typst_render::escape_unescaped_ats`): the example body is `eval`'d as
/// markup, so an unescaped `@` (email/handle) crashes the WHOLE render with
/// the exact ``does not exist in the document`` signature the retry keys on,
/// and escaping it (`\@`) — what the retry does — makes the same input render.
/// If typst ever changes that error text or `@`-handling, this breaks and we
/// learn before the retry silently stops firing.
#[tokio::test]
async fn unescaped_at_crashes_and_escaped_at_renders() {
    if !typst_available() {
        eprintln!("skipping: typst CLI not on PATH");
        return;
    }
    let templates = gw_typst::discover_templates(&example_dir()).expect("discover");
    let letter = templates.iter().find(|t| t.id == "letter").unwrap();
    let with_body = |body: &str| {
        vec![
            ("recipient_name".to_string(), "Acme Co.".to_string()),
            ("recipient_address".to_string(), "123 Main St".to_string()),
            ("subject".to_string(), "Hi".to_string()),
            ("body".to_string(), body.to_string()),
            ("sender_name".to_string(), "Jane Doe".to_string()),
        ]
    };
    // Bare `@` → crash with the signature.
    let err = gw_typst::compile(
        letter,
        &with_body("Dear Ms. Roe,\n\nReach me at jane@example.com."),
        1,
        None,
    )
    .await
    .unwrap_err();
    match err {
        gw_typst::CompileError::Failed(msg) => assert!(
            msg.contains("does not exist in the document"),
            "signature the retry keys on changed: {msg}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    // Escaped `@` → renders (this is what the auto-escape retry produces).
    let rendered = gw_typst::compile(
        letter,
        &with_body("Dear Ms. Roe,\n\nReach me at jane\\@example.com."),
        1,
        None,
    )
    .await
    .expect("escaped @ should compile");
    assert!(rendered.pdf.starts_with(b"%PDF-"), "PDF magic missing");
}
