use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use lsp_types::{CompletionItem, CompletionItemKind, CompletionParams, CompletionTextEdit, CompletionResponse, PartialResultParams, Position, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams};
use odoo_ls_server::core::{file_mgr::FileMgr, odoo::Odoo};
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;

mod setup;
mod test_utils;

use test_utils::position_after;

fn views_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons")
        .join("module_xml_completion").join("views").join("completion_views.xml")
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

/// Menus, actions and templates are declared outside of `<record>`, and complete all the same.
#[test]
fn test_xml_completion_menus_and_templates() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let parents = labels(&mut session, &path, &content, r#"parent="module_xml_completion.completion_menu_ro"#);
    assert_eq!(parents, vec!["module_xml_completion.completion_menu_root".to_string()]);

    let actions = labels(&mut session, &path, &content, r#"action="module_xml_completion.completion_action_par"#);
    assert_eq!(actions, vec!["module_xml_completion.completion_action_parent".to_string()]);

    let templates = labels(&mut session, &path, &content, r#"t-call="module_xml_completion.completion_template_ex"#);
    assert_eq!(templates, vec!["module_xml_completion.completion_template_extra".to_string()]);
}

/// Replace the text of `path` in memory, as typing would, leaving the file on disk untouched.
fn set_content(session: &mut SessionInfo, path: &str, text: &str) {
    static VERSION: AtomicI32 = AtomicI32::new(2);
    let event = [TextDocumentContentChangeEvent { range: None, range_length: None, text: text.to_string() }];
    let version = VERSION.fetch_add(1, Ordering::Relaxed);
    let file_mgr = session.sync_odoo.get_file_mgr();
    file_mgr.borrow_mut().update_file_info(session, path, Some(event.as_slice()), Some(version), false);
}

/// Completion is asked for while the tag is still being typed, so the document rarely parses.
#[test]
fn test_xml_completion_in_unparsable_document() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();

    // A start tag with no `>`, deep in an arch, after a comment and a value that hold markup
    let deep = concat!(
        "<odoo>\n",
        "    <record id=\"completion_view_form\" model=\"ir.ui.view\">\n",
        "        <field name=\"model\">module_xml_completion.parent</field>\n",
        "        <field name=\"arch\" type=\"xml\">\n",
        "            <form string=\"Parent\">\n",
        "                <!-- <field name=\"amount\"> a comment holding markup -->\n",
        "                <group invisible=\"context.get('x') != 'y' and 1 &gt; 0\">\n",
        "                    <field name=\"line_ids\">\n",
        "                        <list>\n",
        "                            <field name=\"line_am",
    );
    set_content(&mut session, &path, deep);
    let subview = labels(&mut session, &path, deep, "<field name=\"line_am");
    assert_eq!(subview, vec!["line_amount".to_string()], "expected a field of the comodel of the unclosed subview");

    // An attribute value with no closing quote, at the top level of the record.
    let unterminated = "<odoo>\n    <record id=\"probe\" model=\"module_xml_completion.pa";
    set_content(&mut session, &path, unterminated);
    let models = labels(&mut session, &path, unterminated, "model=\"module_xml_completion.pa");
    assert!(models.contains(&"module_xml_completion.parent".to_string()), "expected a model name, got: {models:?}");

    // The same field, once in a document that parses and once in one that does not.
    let closed = "<odoo>\n    <record id=\"probe\" model=\"module_xml_completion.parent\">\n        <field name=\"amo\"/>\n    </record>\n</odoo>";
    set_content(&mut session, &path, closed);
    let well_formed = labels(&mut session, &path, closed, "<field name=\"amo");
    let broken = "<odoo>\n    <record id=\"probe\" model=\"module_xml_completion.parent\">\n        <field name=\"amo";
    set_content(&mut session, &path, broken);
    assert_eq!(labels(&mut session, &path, broken, "<field name=\"amo"), well_formed);
    assert_eq!(well_formed, vec!["amount".to_string()]);
}

/// A method reaches an attribute value bare, with the private and dunder ones ranked last.
#[test]
fn test_xml_completion_button_methods() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let (items, _) = complete(&mut session, &path, position_after(&content, r#"<button type="object" name=""#));
    let synthetic = items.iter().filter(|item| item.label.starts_with('<') && item.label.ends_with('>')).collect::<Vec<_>>();
    assert!(synthetic.is_empty(), "a name in angle brackets cannot be typed in an attribute, got: {synthetic:?}");

    let mut sort_text = |needle: &str, label: &str| {
        let (items, _) = complete(&mut session, &path, position_after(&content, needle));
        let item = items.iter().find(|item| item.label == label).unwrap_or_else(|| panic!("{label} not offered, got: {items:?}"));
        item.sort_text.clone().unwrap_or_else(|| panic!("{label} has no sort text"))
    };
    let public = sort_text(r#"<button name="action_completion_co"#, "action_completion_confirm");
    let private = sort_text(r#"<button name="_completion_p"#, "_completion_private");
    let dunder = sort_text(r#"<button name="__completion_d"#, "__completion_dunder__");
    assert!(public < private, "a public method sorts before a private one, got {public} and {private}");
    assert!(private < dunder, "a private method sorts before a dunder one, got {private} and {dunder}");

    let (items, _) = complete(&mut session, &path, position_after(&content, r#"<button name="action_completion_co"#));
    let item = &items[0];
    assert!(!matches!(item.kind, Some(CompletionItemKind::METHOD) | Some(CompletionItemKind::FUNCTION)), "a callable kind makes the client insert parentheses");
    let Some(CompletionTextEdit::Edit(edit)) = item.text_edit.clone() else { panic!("expected a text edit") };
    assert_eq!(edit.new_text, "action_completion_confirm");
}

/// The text of a `<field name="model">` names a model, which the walk already resolves there.
#[test]
fn test_xml_completion_in_field_text() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let res_model = labels(&mut session, &path, &content, r#"<field name="res_model">module_xml_completion.pa"#);
    assert!(res_model.contains(&"module_xml_completion.parent".to_string()), "expected a model name, got: {res_model:?}");

    // A value an element spreads over several lines is replaced without its indentation.
    let (items, _) = complete(&mut session, &path, position_after(&content, "\n            module_xml_completion.par"));
    assert_eq!(items.iter().map(|item| item.label.clone()).collect::<Vec<_>>(), vec!["module_xml_completion.parent".to_string()]);
    let Some(CompletionTextEdit::Edit(edit)) = items[0].text_edit.clone() else { panic!("expected a text edit") };
    assert_eq!(edit.range.start.character, 12);
    assert_eq!(edit.range.end.character - edit.range.start.character, "module_xml_completion.par".len() as u32);

    // Content the parser gives no text node to still completes, capped like any empty value.
    let (items, is_incomplete) = complete(&mut session, &path, position_after(&content, r#"completion_empty_text_probe" model="ir.ui.view"><field name="model">"#));
    assert!(is_incomplete && !items.is_empty() && items.len() <= 200, "expected a capped list, got {} items", items.len());

    // The same text in a document that does not parse yet.
    let broken = "<odoo>\n    <record id=\"probe\" model=\"ir.ui.view\">\n        <field name=\"model\">res.";
    set_content(&mut session, &path, broken);
    let typed = labels(&mut session, &path, broken, "<field name=\"model\">res.");
    assert!(typed.contains(&"res.partner".to_string()), "expected a model name in an unparsable document, got: {typed:?}");
}

/// A `ref=` reaches the comodel records named by a dotted prefix or by the start of a module name.
#[test]
fn test_xml_completion_ref_to_own_module() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = views_path().sanitize();
    let content = std::fs::read_to_string(&path).unwrap();

    let expected = vec!["module_xml_completion.completion_partner".to_string()];
    assert_eq!(labels(&mut session, &path, &content, r#"name="partner_id" ref="module_xml_c"#), expected);
    assert_eq!(labels(&mut session, &path, &content, r#"name="owner_id" ref="module_xml_completion.completion_par"#), expected);
}
