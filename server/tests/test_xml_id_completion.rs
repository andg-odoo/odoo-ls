mod setup;
mod test_utils;

use lsp_types::CompletionResponse;
use odoo_ls_server::core::file_mgr::FileInfo;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::core::symbols::symbol_keys::SourceFileKey;
use odoo_ls_server::features::completion::CompletionFeature;
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

fn completion_labels(
    session: &mut SessionInfo,
    file_symbol: SourceFileKey,
    file_info: &Rc<RefCell<FileInfo>>,
    line: u32,
    character: u32,
) -> Vec<String> {
    match CompletionFeature::autocomplete(session, file_symbol, file_info, None, line, character) {
        Some(CompletionResponse::Array(items)) => items.into_iter().map(|item| item.label).collect(),
        Some(CompletionResponse::List(list)) => list.items.into_iter().map(|item| item.label).collect(),
        None => vec![],
    }
}

/// xml_id string literals complete from the current module and its dependencies.
#[test]
fn test_xml_id_completion() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let test_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("data").join("addons")
        .join("module_1").join("models").join("xml_id_probe.py")
        .sanitize();
    assert!(Path::new(&test_file).exists(), "Test file does not exist: {test_file}");
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_info = file_mgr.borrow().get_file_info(&test_file).unwrap();
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&test_file))
        .expect("Failed to get file symbol");

    // env.ref("module_1.") -> the xml_ids declared by the current module
    let refs = completion_labels(&mut session, file_symbol, &file_info, 12, 31);
    assert!(
        refs.contains(&"module_1.test_xml_test_record".to_string()),
        "env.ref should complete the xml_ids of the current module, got: {refs:?}"
    );

    // groups="module_1." -> none of them, as none is a res.groups record
    let module_groups = completion_labels(&mut session, file_symbol, &file_info, 9, 53);
    assert!(
        module_groups.is_empty(),
        "groups= should only complete res.groups records, got: {module_groups:?}"
    );

    // groups="base.group_" -> the groups of base, out of the dependencies of the current module
    let base_groups = completion_labels(&mut session, file_symbol, &file_info, 8, 54);
    assert!(
        base_groups.contains(&"base.group_user".to_string()),
        "groups= should complete the res.groups records of base, got: {base_groups:?}"
    );

    // has_group("base.group_") on a res.users recordset -> same candidates
    let has_group = completion_labels(&mut session, file_symbol, &file_info, 13, 62);
    assert!(
        has_group.contains(&"base.group_user".to_string()),
        "has_group should complete the res.groups records of base, got: {has_group:?}"
    );
}
