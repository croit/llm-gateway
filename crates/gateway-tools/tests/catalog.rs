// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Tool-catalog grouping, toggle keys and RBAC-filtered entries.
//!
//! These live here rather than beside the code they exercise because they span
//! both layers: the machinery under test is in `gateway-core`, but the fixtures
//! are the *real* concrete tools, which are in `gateway-tools`. A unit test
//! inside `gateway-core` can't reach them — and swapping in invented doubles
//! would hide exactly the id/grouping drift these tests exist to catch.
//! (`echo` / `get_current_timestamp` stay in `gateway-core` as the canonical
//! trivial tools, so the tests that only need *a* registered tool stay put.)

mod catalog_tests {
    use gateway_core::server::tools::ToolRegistry;
    use gateway_core::server::tools::catalog::*;
    use gateway_core::server::tools::echo::Echo;
    use gateway_core::server::tools::time::CurrentTimestamp;
    use gateway_tools::search_web::SearchWeb;
    use std::collections::HashSet;

    #[test]
    fn each_typst_template_is_its_own_key_variants_collapse_to_it() {
        // Different templates → different keys (independently selectable).
        assert_eq!(entry_key_for("typst_invoice"), "typst_invoice");
        assert_eq!(entry_key_for("typst_report"), "typst_report");
        // A template's render + edit/read/pptx variants all collapse to its
        // render id, so one toggle governs that template's whole family.
        assert_eq!(entry_key_for("typst_invoice_edit"), "typst_invoice");
        assert_eq!(entry_key_for("typst_invoice_read"), "typst_invoice");
        assert_eq!(entry_key_for("typst_invoice_pptx"), "typst_invoice");
        assert_eq!(entry_key_for("search_web"), "search_web");
    }

    #[test]
    fn entries_emit_one_row_per_template_with_manifest_title() {
        let reg = ToolRegistry::new();
        // Render + variants of two templates, all RBAC-granted.
        let allowed = vec![
            "typst_letter".to_string(),
            "typst_letter_edit".to_string(),
            "typst_letter_read".to_string(),
            "typst_invoice".to_string(),
        ];
        let metas = vec![TemplateMeta {
            key: "typst_letter".to_string(),
            title: "Formal letter".to_string(),
            description: "A business letter. Renders to PDF.".to_string(),
        }];
        let rows = entries(&reg, &allowed, &metas, &[]);
        // One row per template (variants folded), both under Templates.
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|e| e.category == Category::Templates));
        let letter = rows.iter().find(|e| e.key == "typst_letter").unwrap();
        // Manifest title + first sentence of its description.
        assert_eq!(letter.title, "Formal letter");
        assert_eq!(letter.description, "A business letter.");
        // No manifest meta → prettified id fallback.
        let invoice = rows.iter().find(|e| e.key == "typst_invoice").unwrap();
        assert_eq!(invoice.title, "Invoice");
    }

    #[test]
    fn requires_chat_session_covers_session_only_tools() {
        // The typst render family (every template + its edit/read/pptx
        // variants) and the document-canvas tools + upload_attachment hard-fail
        // off the chat path, so they must be reported session-only.
        for id in [
            "typst_letter",
            "typst_letter_edit",
            "typst_letter_read",
            "typst_letter_pptx",
            "typst_presentation",
            "create_document",
            "edit_document",
            "read_document",
            "list_documents",
            "edit_document_section",
            "export_document",
            "list_document_versions",
            "restore_document_version",
            "upload_attachment",
            "list_attachments",
            // generate_image / generate_qr_code splice an attachment marker
            // into the turn, so they also need a live chat session.
            "generate_image",
            "generate_qr_code",
        ] {
            assert!(requires_chat_session(id), "{id} should be session-only");
        }
        // Tools that work on the proxy path (no session) must NOT be dropped —
        // notably read_skill (explicitly proxy-safe), the sandbox tools that
        // return content rather than a turn attachment, and plain retrieval.
        for id in [
            "read_skill",
            "search_web",
            "fetch_url",
            "fetch_attachment",
            "generate_document",
            "render_typst",
            "run_in_sandbox",
            "rag_search",
            "remember",
        ] {
            assert!(
                !requires_chat_session(id),
                "{id} must stay on the proxy path"
            );
        }
    }

    #[test]
    fn prettify_humanises_slugs() {
        assert_eq!(prettify("quarterly_report"), "Quarterly report");
        assert_eq!(prettify("letter"), "Letter");
        assert_eq!(prettify("a-b-c"), "A b c");
    }

    #[test]
    fn capability_domains_derive_from_registry_and_skip_hidden() {
        use gateway_tools::rag::RagSearch;
        let reg = ToolRegistry::new()
            .with(SearchWeb)
            .with(RagSearch::new(std::sync::Arc::new(
                gateway_core::server::rbac::Resolver::empty(),
            )))
            .with(Echo);
        // Echo (`company_echo`) is hidden → its area never shows. Order is by
        // Category::order: Web before Knowledge.
        assert_eq!(
            capability_domains(&reg),
            vec!["Web & Network", "Knowledge base"]
        );
    }

    #[test]
    fn rag_search_gets_a_curated_entry_in_the_knowledge_area() {
        use gateway_tools::rag::RagSearch;
        let reg = ToolRegistry::new().with(RagSearch::new(std::sync::Arc::new(
            gateway_core::server::rbac::Resolver::empty(),
        )));
        let entries = entries(&reg, &["rag_search".to_string()], &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, Category::Knowledge);
        // Curated title from `display_meta`, not the raw schema function name.
        assert_eq!(entries[0].title, "Knowledge-base search");
    }

    #[test]
    fn entries_hide_smoke_test_tools() {
        let reg = ToolRegistry::new().with(Echo).with(SearchWeb);
        let allowed = vec!["company_echo".to_string(), "search_web".to_string()];
        let entries = entries(&reg, &allowed, &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "search_web");
        assert_eq!(entries[0].category, Category::Web);
    }

    #[test]
    fn entries_are_grouped_then_sorted_by_key() {
        let reg = ToolRegistry::new().with(SearchWeb).with(CurrentTimestamp);
        let allowed = vec![
            "get_current_timestamp".to_string(),
            "search_web".to_string(),
        ];
        let entries = entries(&reg, &allowed, &[], &[]);
        // Web (search_web) sorts before Utility (get_current_timestamp).
        assert_eq!(entries[0].key, "search_web");
        assert_eq!(entries[1].key, "get_current_timestamp");
        assert_eq!(entries[1].category, Category::Utility);
    }

    #[test]
    fn retain_enabled_drops_disabled_and_keeps_rest() {
        let mut allowed = vec![
            "search_web".to_string(),
            "fetch_url".to_string(),
            "typst_invoice".to_string(),
        ];
        let disabled: HashSet<String> = ["search_web".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url", "typst_invoice"]);
    }

    #[test]
    fn remember_and_recall_collapse_to_one_memory_entry() {
        use gateway_tools::memory::{Recall, Remember};
        let reg = ToolRegistry::new().with(Remember).with(Recall);
        let allowed = vec!["remember".to_string(), "recall".to_string()];
        let entries = entries(&reg, &allowed, &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "memory");
        assert_eq!(entries[0].category, Category::Memory);
    }

    #[test]
    fn both_memory_ids_map_to_the_memory_key() {
        assert_eq!(entry_key_for("remember"), "memory");
        assert_eq!(entry_key_for("recall"), "memory");
    }

    #[test]
    fn disabling_memory_key_drops_both_tools() {
        let mut allowed = vec![
            "remember".to_string(),
            "recall".to_string(),
            "fetch_url".to_string(),
        ];
        let disabled: HashSet<String> = ["memory".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url"]);
    }

    #[test]
    fn document_tools_collapse_to_one_document_entry() {
        use gateway_tools::document::{CreateDocument, EditDocument, ListDocuments, ReadDocument};
        let reg = ToolRegistry::new()
            .with(CreateDocument)
            .with(EditDocument)
            .with(ReadDocument)
            .with(ListDocuments);
        let allowed = vec![
            "create_document".to_string(),
            "edit_document".to_string(),
            "read_document".to_string(),
            "list_documents".to_string(),
        ];
        let entries = entries(&reg, &allowed, &[], &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "document");
        assert_eq!(entries[0].category, Category::Documents);
    }

    #[test]
    fn all_document_ids_map_to_the_document_key() {
        for id in [
            "create_document",
            "edit_document",
            "read_document",
            "list_documents",
            "export_document",
            "edit_document_section",
            "list_document_versions",
            "restore_document_version",
        ] {
            assert_eq!(entry_key_for(id), "document", "{id}");
        }
        // The sandbox document tools must NOT collapse into this key.
        assert_eq!(entry_key_for("generate_document"), "generate_document");
        assert_eq!(entry_key_for("convert_document"), "convert_document");
    }

    #[test]
    fn disabling_document_key_drops_all_canvas_tools() {
        let mut allowed = vec![
            "create_document".to_string(),
            "edit_document".to_string(),
            "read_document".to_string(),
            "list_documents".to_string(),
            "fetch_url".to_string(),
        ];
        let disabled: HashSet<String> = ["document".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url"]);
    }

    #[test]
    fn disabling_one_template_drops_its_family_but_not_other_templates() {
        let mut allowed = vec![
            "typst_invoice".to_string(),
            "typst_invoice_edit".to_string(),
            "typst_invoice_read".to_string(),
            "typst_report".to_string(),
            "fetch_url".to_string(),
        ];
        // Disabling one template's key drops its render + every variant…
        let disabled: HashSet<String> = ["typst_invoice".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        // …but leaves a different template (and unrelated tools) intact.
        assert_eq!(allowed, vec!["typst_report", "fetch_url"]);
    }

    #[test]
    fn mcp_tools_collapse_to_one_entry_per_server() {
        // The MCP branch builds its entry from the id alone (no registry
        // lookup), so an empty registry is fine here.
        let reg = ToolRegistry::new();
        let allowed = vec![
            "mcp__demo__echo".to_string(),
            "mcp__demo__get-sum".to_string(),
            "mcp__other__ping".to_string(),
        ];
        let mcp: Vec<_> = entries(&reg, &allowed, &[], &[])
            .into_iter()
            .filter(|e| e.category == Category::Integrations)
            .collect();
        assert_eq!(mcp.len(), 2, "two servers → two entries: {mcp:?}");
        assert!(mcp.iter().any(|e| e.key == "mcp__demo"));
        assert!(mcp.iter().any(|e| e.key == "mcp__other"));
    }

    #[test]
    fn disabling_an_mcp_server_drops_all_its_tools() {
        // Guards the display-key (`entry_key_for`) ↔ toggle-key consistency:
        // the `/tools` row is keyed `mcp__demo`, so disabling it must drop
        // every `mcp__demo__*` id.
        let mut allowed = vec![
            "mcp__demo__echo".to_string(),
            "mcp__demo__get-sum".to_string(),
            "search_web".to_string(),
        ];
        let disabled: HashSet<String> = ["mcp__demo".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["search_web"]);
    }
}
