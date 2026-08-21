use std::path::Path;

use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::{core::{file_mgr::FileMgr, odoo::Odoo}, threads::SessionInfo, utils::{PathSanitizer, ToFilePath}};

use crate::setup::setup::{create_init_session, setup_server};
use crate::test_utils::position_after;

mod setup;
mod test_utils;

/// The names of a modifier expression are fields of the model the element resolves against.
#[test]
fn test_xml_modifier_fields() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_xml_subview");
    let views = module.join("views").join("subview_views.xml").sanitize();
    let models = module.join("models").join("subview_models.py").sanitize();
    let content = std::fs::read_to_string(&views).unwrap();

    // A bare name is a field of the record model, the predefined ones naming nothing.
    let position = position_after(&content, r#"invisible="not tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[9]);
    let position = position_after(&content, r#"not tag_id or uid"#);
    assert_definitions(&mut session, &views, position, &models, &[]);

    // The modifiers of a `<field>` holding a subview belong to the model above it, entity and all.
    let position = position_after(&content, r#"readonly="amount"#);
    assert_definitions(&mut session, &views, position, &models, &[7]);

    // Inside the subview a name is a field of the comodel, and `parent.` one of the model above.
    let position = position_after(&content, r#"decoration-danger="amount"#);
    assert_definitions(&mut session, &views, position, &models, &[17]);
    let position = position_after(&content, r#"column_invisible="parent.amount"#);
    assert_definitions(&mut session, &views, position, &models, &[7]);

    // An expression that does not parse resolves to nothing rather than to a diagnostic.
    let position = position_after(&content, r#"invisible="tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[]);
}

/// A domain names fields of the model it targets, and values of the model in scope.
#[test]
fn test_xml_domain_fields() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_xml_subview");
    let views = module.join("views").join("subview_views.xml").sanitize();
    let models = module.join("models").join("subview_models.py").sanitize();
    let content = std::fs::read_to_string(&views).unwrap();

    // The left-hand side of a leaf is a field of the comodel of the `<field>` carrying the domain.
    let position = position_after(&content, r#"line_ids" domain="[('amount"#);
    assert_definitions(&mut session, &views, position, &models, &[17]);
    // A dotted one walks the comodels, each segment resolving on its own.
    let position = position_after(&content, r#", ('parent_id"#);
    assert_definitions(&mut session, &views, position, &models, &[18]);
    let position = position_after(&content, r#"('parent_id.tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[9]);

    // A value is a field of the model in scope, and `parent.` one of the model above.
    let position = position_after(&content, r#"'=', amount"#);
    assert_definitions(&mut session, &views, position, &models, &[7]);
    let position = position_after(&content, r#"'=', parent.amount"#);
    assert_definitions(&mut session, &views, position, &models, &[7]);
    // In that subview the comodel of `parent_id` is the model the left-hand side resolves against.
    let position = position_after(&content, r#"parent_id" domain="[('tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[9]);

    // Only a `<field>` retargets, a `<filter>` naming the fields of the view's own model.
    let position = position_after(&content, r#"tagged" domain="[('tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[9]);

    // A domain that does not parse resolves to nothing rather than to a diagnostic.
    let position = position_after(&content, r#"tag_id" domain="[('amount"#);
    assert_definitions(&mut session, &views, position, &models, &[]);
}

/// A context names a field of the target model in a `default_` key, and of its own in `group_by`.
#[test]
fn test_xml_context_fields() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_xml_subview");
    let views = module.join("views").join("subview_views.xml").sanitize();
    let models = module.join("models").join("subview_models.py").sanitize();
    let content = std::fs::read_to_string(&views).unwrap();

    // The default lands on a record of the comodel, while the grouping is on the model in scope.
    let position = position_after(&content, r#"{'default_amount"#);
    assert_definitions(&mut session, &views, position, &models, &[17]);
    let position = position_after(&content, r#"'group_by': 'amount"#);
    assert_definitions(&mut session, &views, position, &models, &[7]);

    // A value of the context is a field of the model in scope, as in a modifier.
    let position = position_after(&content, r#"'default_amount': tag_id"#);
    assert_definitions(&mut session, &views, position, &models, &[9]);
}

fn assert_definitions(session: &mut SessionInfo, path: &str, position: Position, target_path: &str, expected_lines: &[u32]) {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let links = match Odoo::handle_goto_definition(session, params) {
        Ok(None) => vec![],
        Ok(Some(GotoDefinitionResponse::Link(links))) => links,
        other => panic!("expected definition links at {}:{}, got {other:?}", position.line + 1, position.character + 1),
    };
    let found = links.iter()
        .map(|link| (link.target_uri.to_file_path().unwrap().sanitize(), link.target_range.start.line))
        .collect::<Vec<_>>();
    let expected = expected_lines.iter().map(|line| (target_path.to_string(), *line)).collect::<Vec<_>>();
    assert_eq!(found, expected, "unexpected definitions at {}:{}", position.line + 1, position.character + 1);
}
