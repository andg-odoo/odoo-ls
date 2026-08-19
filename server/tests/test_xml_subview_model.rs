use std::path::Path;

use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::{core::{file_mgr::FileMgr, odoo::Odoo}, threads::SessionInfo, utils::{PathSanitizer, ToFilePath}};

use crate::setup::setup::{create_init_session, setup_server};

mod setup;

/// A field inside an inline subview belongs to the comodel, not to the enclosing record.
#[test]
fn test_xml_subview_model() {
    let (mut odoo, config) = setup_server(true);
    let mut session = create_init_session(&mut odoo, config);
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons").join("module_xml_subview");
    let views = module.join("views").join("subview_views.xml").sanitize();
    let models = module.join("models").join("subview_models.py").sanitize();

    // Directly under the arch: the model of the record.
    assert_definitions(&mut session, &views, Position::new(6, 29), &models, &[7]);
    // Inside the inline list of a one2many: its comodel.
    assert_definitions(&mut session, &views, Position::new(9, 37), &models, &[17]);
    // `<groupby>` names a field of the enclosing model...
    assert_definitions(&mut session, &views, Position::new(27, 31), &models, &[9]);
    // ...and its content resolves against that field's comodel.
    assert_definitions(&mut session, &views, Position::new(28, 33), &models, &[25]);
    // A subview with no resolvable comodel resolves to nothing rather than to the parent.
    assert_definitions(&mut session, &views, Position::new(14, 37), &models, &[]);
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
