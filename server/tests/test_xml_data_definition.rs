use std::path::Path;

use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, HoverParams, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::{core::{file_mgr::FileMgr, odoo::Odoo}, threads::SessionInfo, utils::{PathSanitizer, ToFilePath}};

mod setup;
mod test_utils;

use test_utils::{line_of, position_after};

fn views_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons")
        .join("module_xml_completion").join("views").join("completion_views.xml")
        .sanitize()
}

/// `<menuitem>` and `<template>` declare xml ids too, and are navigated to like a `<record>`.
#[test]
fn test_xml_data_definition() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let views = views_path();
    let content = std::fs::read_to_string(&views).unwrap();

    assert_definition(
        &mut session, &views,
        position_after(&content, r#"parent="module_xml_completion.completion_menu_other"#),
        line_of(&content, r#"<menuitem id="completion_menu_other""#),
    );
    assert_definition(
        &mut session, &views,
        position_after(&content, r#"t-call="module_xml_completion.completion_template_base"#),
        line_of(&content, r#"<template id="completion_template_base">"#),
    );
}

fn assert_definition(session: &mut SessionInfo, path: &str, position: Position, expected_line: u32) {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let links = match Odoo::handle_goto_definition(session, params) {
        Ok(Some(GotoDefinitionResponse::Link(links))) => links,
        other => panic!("expected definition links at {}:{}, got {other:?}", position.line + 1, position.character + 1),
    };
    let found = links.iter()
        .map(|link| (link.target_uri.to_file_path().unwrap().sanitize(), link.target_range.start.line))
        .collect::<Vec<_>>();
    assert_eq!(found, vec![(path.to_string(), expected_line)], "unexpected definitions at {}:{}", position.line + 1, position.character + 1);
}

/// Hovering those same references renders them rather than tripping over their missing name.
#[test]
fn test_xml_data_hover() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let views = views_path();
    let content = std::fs::read_to_string(&views).unwrap();

    for needle in [
        r#"parent="module_xml_completion.completion_menu_other"#,
        r#"t-call="module_xml_completion.completion_template_base"#,
        r#"inherit_id="module_xml_completion.completion_template_base"#,
    ] {
        let position = position_after(&content, needle);
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(&views) },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let hover = Odoo::handle_hover(&mut session, params).expect("hover failed");
        assert!(hover.is_some(), "expected a hover at {needle:?}");
    }
}
