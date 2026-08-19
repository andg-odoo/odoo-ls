use std::path::Path;

use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
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

/// Every xml data symbol a reference resolves to renders as its own block, not as a bare `repr`.
#[test]
fn test_xml_data_hover() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let views = views_path();
    let content = std::fs::read_to_string(&views).unwrap();

    let menu = hover_at(&mut session, &views, position_after(&content, r#"parent="module_xml_completion.completion_menu_other"#));
    assert!(menu.contains("(XML menuitem) module_xml_completion.completion_menu_other"), "got: {menu}");
    assert!(menu.contains("file: completion_views.xml"), "got: {menu}");

    // A template shows what it is named and what it extends.
    let template = hover_at(&mut session, &views, position_after(&content, r#"t-call="module_xml_completion.completion_template_child"#));
    assert!(template.contains("(XML template) module_xml_completion.completion_template_child"), "got: {template}");
    assert!(template.contains("t-name: completion_child"), "got: {template}");
    assert!(template.contains("inherits: module_xml_completion.completion_template_base"), "got: {template}");

    // An asset keeps to its id, its bundle and path being elements the builder does not store.
    let asset = hover_at(&mut session, &views, position_after(&content, r#"ref="module_xml_completion.completion_asset"#));
    assert!(asset.contains("(XML asset) module_xml_completion.completion_asset"), "got: {asset}");
    assert!(asset.contains("file: completion_views.xml"), "got: {asset}");

    // A `<delete>` names records to remove, so it contributes nothing to the id it claims.
    let deleted = hover_at(&mut session, &views, position_after(&content, r#"ref="module_xml_completion.completion_partner"#));
    assert!(deleted.contains("(XML record) module_xml_completion.completion_partner"), "got: {deleted}");
    assert!(!deleted.contains("delete"), "a delete must not be a hover subject, got: {deleted}");

    // A template with no t-name or t-inherit keeps the block down to what it has.
    let base = hover_at(&mut session, &views, position_after(&content, r#"inherit_id="module_xml_completion.completion_template_base"#));
    assert!(base.contains("(XML template) module_xml_completion.completion_template_base"), "got: {base}");
    assert!(!base.contains("t-name:") && !base.contains("inherits:"), "empty lines should be omitted, got: {base}");
}

fn hover_at(session: &mut SessionInfo, path: &str, position: Position) -> String {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    match Odoo::handle_hover(session, params) {
        Ok(Some(Hover { contents: HoverContents::Markup(markup), .. })) => markup.value,
        other => panic!("expected a hover at {}:{}, got {other:?}", position.line + 1, position.character + 1),
    }
}
