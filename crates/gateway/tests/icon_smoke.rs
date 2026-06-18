// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Sanity-check the inline-SVG icons emit the expected markup.
use session_core::icons;

#[test]
fn mic_icon_carries_explicit_width_height_and_viewbox() {
    let html = icons::mic(16).to_string();
    eprintln!("---icon markup---\n{html}\n---");
    assert!(
        html.contains("width=\"16\"") && html.contains("height=\"16\""),
        "expected explicit width/height: {html}"
    );
    assert!(
        html.contains("viewBox=\"0 0 24 24\""),
        "viewBox missing: {html}"
    );
    assert!(html.contains("<rect"), "expected rect element");
}

#[test]
fn nav_icons_all_render_at_the_same_size() {
    // The header nav passes the same size for every icon — assert that
    // the rendered markup is consistent so we don't ship a navbar where
    // each link's icon visibly differs.
    let s = 16;
    let cases = [
        icons::home(s).to_string(),
        icons::key(s).to_string(),
        icons::message(s).to_string(),
    ];
    for case in &cases {
        assert!(
            case.contains("width=\"16\"") && case.contains("height=\"16\""),
            "icon missing width/height: {case}"
        );
    }
}
