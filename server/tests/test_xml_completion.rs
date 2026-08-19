use std::path::{Path, PathBuf};

use lsp_types::{CompletionItem, CompletionParams, CompletionTextEdit, CompletionResponse, PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::core::{file_mgr::FileMgr, odoo::Odoo};
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;
mod test_utils;

fn views_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons")
        .join("module_xml_completion").join("views").join("completion_views.xml")
}

/// Cursor position right after the single occurrence of `needle`, where completion is asked for.
fn position_after(content: &str, needle: &str) -> Position {
    let start = content.find(needle).unwrap_or_else(|| panic!("{needle:?} not found in the fixture"));
    assert!(content[start + 1..].find(needle).is_none(), "{needle:?} is not unique in the fixture");
    let offset = start + needle.len();
    let line = content[..offset].matches('\n').count();
    let character = offset - content[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
    Position::new(line as u32, character as u32)
}

fn complete(session: &mut SessionInfo, path: &str, position: Position) -> (Vec<CompletionItem>, bool) {
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: FileMgr::pathname2uri(path) },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    };
    match Odoo::handle_autocomplete(session, params) {
        Ok(None) => (vec![], false),
        Ok(Some(CompletionResponse::Array(items))) => (items, false),
        Ok(Some(CompletionResponse::List(list))) => (list.items, list.is_incomplete),
        Err(error) => panic!("completion failed at {}:{}: {error:?}", position.line + 1, position.character + 1),
    }
}

fn labels(session: &mut SessionInfo, path: &str, content: &str, needle: &str) -> Vec<String> {
    let (items, _) = complete(session, path, position_after(content, needle));
    items.into_iter().map(|item| item.label).collect()
}

#[test]
fn test_xml_completion() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    // `<record model=…>` completes model names.
    let models = labels(&mut session, &path, &content, r#"completion_model_probe" model="module_xml_completion.pa"#);
    assert!(models.contains(&"module_xml_completion.parent".to_string()), "expected a model name, got: {models:?}");

    // A `<field name=…>` directly under a `<record>` completes the fields of the record model.
    let record_fields = labels(&mut session, &path, &content, r#"<field name="res_"#);
    assert!(record_fields.contains(&"res_model".to_string()), "expected a field of ir.actions.act_window, got: {record_fields:?}");

    // Inside the arch, fields resolve against the model the view targets, replacing the value.
    let (items, _) = complete(&mut session, &path, position_after(&content, r#"<field name="amo"#));
    assert_eq!(items.iter().map(|item| item.label.clone()).collect::<Vec<_>>(), vec!["amount".to_string()]);
    let Some(CompletionTextEdit::Edit(edit)) = items[0].text_edit.clone() else { panic!("expected a text edit") };
    assert_eq!(edit.new_text, "amount");
    assert_eq!(edit.range.end.character - edit.range.start.character, 3);

    // An inline subview resolves against the comodel of the field holding it.
    let subview_fields = labels(&mut session, &path, &content, r#"<field name="line_am"#);
    assert_eq!(subview_fields, vec!["line_amount".to_string()], "expected a field of the comodel");

    // `<field name="model">` may come after the arch it targets.
    let unordered_fields = labels(&mut session, &path, &content, r#"<field name="tot"#);
    assert_eq!(unordered_fields, vec!["total".to_string()], "expected the arch to target the model of its record");

    // `<button name=…>` completes the methods of the model in scope.
    let methods = labels(&mut session, &path, &content, r#"<button name="action_completion_co"#);
    assert_eq!(methods, vec!["action_completion_confirm".to_string()], "expected a method of the target model");
}

/// The field type, and the comodel of a relational field, annotate the completion item.
#[test]
fn test_xml_completion_field_details() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let (items, _) = complete(&mut session, &path, position_after(&content, r#"<field name="line_am"#));
    let description = items[0].label_details.as_ref().and_then(|details| details.description.clone());
    assert_eq!(description, Some("Float".to_string()));

    let (items, _) = complete(&mut session, &path, position_after(&content, r#"<field name="line_ids"#));
    let description = items[0].label_details.as_ref().and_then(|details| details.description.clone());
    assert_eq!(description, Some("One2many(module_xml_completion.line)".to_string()));
}

/// An empty value completes everything the position accepts, capped and flagged incomplete.
#[test]
fn test_xml_completion_cap() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let (items, is_incomplete) = complete(&mut session, &path, position_after(&content, r#"completion_model_probe" model=""#));
    assert!(is_incomplete, "a capped response must be flagged incomplete");
    assert!(items.len() <= 200, "expected at most 200 items, got {}", items.len());
}

/// Attributes taking an xml id complete from the ids the position accepts.
#[test]
fn test_xml_completion_xml_ids() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    // `ref=` is filtered to the comodel of the field carrying it.
    let groups = labels(&mut session, &path, &content, r#"name="group_id" ref="base.group_us"#);
    assert!(groups.contains(&"base.group_user".to_string()), "expected a res.groups record, got: {groups:?}");
    let partners = labels(&mut session, &path, &content, r#"name="partner_id" ref="base.group_us"#);
    assert!(partners.is_empty(), "a res.partner ref should not offer res.groups records, got: {partners:?}");

    // `groups=` completes the segment under the cursor, past the `!` that negates it.
    let position = position_after(&content, r#"groups="base.group_user,!base.group_us"#);
    let (items, _) = complete(&mut session, &path, position);
    assert!(items.iter().any(|item| item.label == "base.group_user"), "expected a res.groups record, got: {items:?}");
    let Some(CompletionTextEdit::Edit(edit)) = items[0].text_edit.clone() else { panic!("expected a text edit") };
    assert_eq!(edit.range.end.character - edit.range.start.character, "base.group_us".len() as u32);
}
