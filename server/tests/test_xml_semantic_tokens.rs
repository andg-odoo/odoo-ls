use std::path::{Path, PathBuf};

use lsp_types::SemanticToken;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::features::semantic_tokens::SemanticTokensFeature;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;
mod test_utils;

// Indices into `SemanticTokensFeature::legend()`.
const CLASS: u32 = 0;
const TYPE: u32 = 5;
const PROPERTY: u32 = 9;
const NO_MODIFIER: u32 = 0;
const DECLARATION: u32 = 1 << 0;

fn addon_path(segments: &[&str]) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join("addons");
    path.extend(segments);
    path
}

/// Every token of `path` as (source text, type, modifiers), decoded from the delta encoding.
fn tokens_of(session: &mut SessionInfo, path: &Path) -> Vec<(String, u32, u32)> {
    let content = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    let sanitized = path.sanitize();
    let file_info = session.sync_odoo.get_file_mgr().borrow().get_file_info(&sanitized).unwrap_or_else(|| panic!("no file info for {sanitized}"));
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(session, path).unwrap();

    let tokens = SemanticTokensFeature::tokens_xml(session, file_symbol, &file_info);
    let (mut line, mut character) = (0usize, 0usize);
    let mut decoded = vec![];
    for SemanticToken { delta_line, delta_start, length, token_type, token_modifiers_bitset } in tokens.data {
        line += delta_line as usize;
        character = match delta_line {
            0 => character + delta_start as usize,
            _ => delta_start as usize,
        };
        let text = &lines[line][character..character + length as usize];
        decoded.push((text.to_string(), token_type, token_modifiers_bitset));
    }
    decoded
}

fn assert_token(tokens: &[(String, u32, u32)], text: &str, token_type: u32, modifiers: u32) {
    assert!(
        tokens.iter().any(|token| token == &(text.to_string(), token_type, modifiers)),
        "Expected token ({text:?}, type {token_type}, modifiers {modifiers}), got: {tokens:?}"
    );
}

#[test]
fn test_xml_semantic_tokens() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = addon_path(&["module_xml_models_fields", "data", "x_test_model_views.xml"]);

    let tokens = tokens_of(&mut session, &path);

    // `<record id=…>` declares its xml id, `<menuitem action=…>` references one.
    assert_token(&tokens, "view_test_model_tree", CLASS, DECLARATION);
    assert_token(&tokens, "action_test_model", CLASS, DECLARATION);
    assert_token(&tokens, "action_test_model", CLASS, NO_MODIFIER);
    // Model names: the `<record model=…>` itself, and the one a `<field name="model">` names.
    assert_token(&tokens, "ir.ui.view", CLASS, NO_MODIFIER);
    assert_token(&tokens, "x_test_model", CLASS, NO_MODIFIER);
    // Fields of the record model, and of the model the arch targets.
    assert_token(&tokens, "res_model", PROPERTY, NO_MODIFIER);
    assert_token(&tokens, "x_name", PROPERTY, NO_MODIFIER);
    // Text content is only resolved where it names a model: the view name is not one.
    assert!(
        !tokens.iter().any(|(text, _, _)| text == "x_test_model.tree"),
        "Expected no token for a text content that is not a model, got: {tokens:?}"
    );
}

/// OWL template-name tokens keep their own rule, a `t-call` colouring only when it resolves.
#[test]
fn test_xml_semantic_tokens_template_names() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = addon_path(&["module_templates_b", "static", "src", "frontend_templates.xml"]);

    let tokens = tokens_of(&mut session, &path);

    assert_token(&tokens, "module_templates_b.CallFrontend", TYPE, DECLARATION);
    assert_token(&tokens, "module_templates_a.FrontendTemplate", TYPE, NO_MODIFIER);
}

/// A `<template id=…>` declares an xml id like a `<record id=…>` does, and a `t-call` uses one.
#[test]
fn test_xml_semantic_tokens_templates() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = addon_path(&["module_xml_completion", "views", "completion_views.xml"]);

    let tokens = tokens_of(&mut session, &path);

    assert_token(&tokens, "completion_template_base", CLASS, DECLARATION);
    assert_token(&tokens, "module_xml_completion.completion_template_base", CLASS, NO_MODIFIER);
}

/// `groups` is a list, resolved per segment on any element and through `!` negation.
#[test]
fn test_xml_semantic_tokens_groups() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = addon_path(&["module_xml_completion", "views", "completion_views.xml"]);

    let tokens = tokens_of(&mut session, &path);

    // On the `<template>` itself, on a plain `<t>` child, and on a `<group>` in a view arch.
    assert_token(&tokens, "base.group_user", CLASS, NO_MODIFIER);
    assert_token(&tokens, "base.group_system", CLASS, NO_MODIFIER);
    // The range covers the id alone, never the `!` or the padding around it.
    assert!(
        !tokens.iter().any(|(text, _, _)| text.contains('!') || text.trim() != text),
        "a group token must exclude the negation and the whitespace, got: {tokens:?}"
    );
    // The second segment of `base.group_user,!base.group_us` names nothing and stays uncoloured.
    assert!(
        !tokens.iter().any(|(text, _, _)| text == "base.group_us"),
        "an unresolved group must not be coloured, got: {tokens:?}"
    );
}

/// A `t-call` on a template of a module that is not a dependency stays uncoloured.
#[test]
fn test_xml_semantic_tokens_out_of_deps_template() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = addon_path(&["module_templates_b", "data", "backend_templates.xml"]);

    let tokens = tokens_of(&mut session, &path);

    assert_token(&tokens, "caller_ols05074", CLASS, DECLARATION);
    assert!(
        !tokens.iter().any(|(text, _, _)| text.starts_with("module_templates_a")),
        "module_templates_b does not depend on module_templates_a, got: {tokens:?}"
    );
}
