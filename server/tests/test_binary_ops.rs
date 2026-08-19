mod setup;
mod test_utils;

use odoo_ls_server::core::file_mgr::FileInfo;
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::core::symbols::symbol_keys::{SourceFileKey, SymbolKey};
use odoo_ls_server::threads::SessionInfo;
use odoo_ls_server::utils::PathSanitizer;
use std::cell::RefCell;
use std::env;
use std::path::Path;
use std::rc::Rc;
use test_utils::get_resolved_symbols_at_position;

/// Loads `binary_ops.py`, resolves `MyClass` and hands both to `f`.
fn with_binary_ops_fixture<F>(f: F)
where
    F: FnOnce(&mut SessionInfo, &Rc<RefCell<FileInfo>>, SourceFileKey, SymbolKey),
{
    let (mut odoo, config) = setup::setup::setup_server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = env::current_dir()
        .unwrap()
        .join("tests/data/python/expressions/binary_ops.py")
        .sanitize();
    setup::setup::prepare_custom_entry_point(&mut session, path.as_str());

    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_info = file_mgr.borrow().get_file_info(&path).unwrap();
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&path))
        .expect("Failed to get file symbol");

    let my_class = session.sync_odoo.get_symbol(path.as_str(), (&[], &["MyClass"]), u32::MAX);
    assert!(!my_class.is_empty(), "MyClass should be found in the test file");
    let my_class = my_class[0];

    f(&mut session, &file_info, file_symbol, my_class);
}

/// Test that `a + b` takes its type from the return type of `a.__add__`.
#[test]
fn test_binary_op_add_uses_dunder_return_type() {
    with_binary_ops_fixture(|session, file_info, file_symbol, my_class| {
        // Line 11: `added` is `Vector() + Vector()`, and `__add__` is annotated `-> MyClass`.
        let resolved = get_resolved_symbols_at_position(session, file_symbol, file_info, 11, 0);
        assert!(
            resolved.len() == 1 && resolved[0] == my_class,
            "Vector() + Vector() should resolve to MyClass via __add__, got: {:?}",
            resolved.iter().map(|&s| session.st().name(s).to_string()).collect::<Vec<_>>()
        );
    });
}

/// Bitwise operators dispatch to their own dunder, as recordsets use `&` and `|`.
#[test]
fn test_binary_op_bitand_uses_dunder_return_type() {
    with_binary_ops_fixture(|session, file_info, file_symbol, my_class| {
        // Line 14: `intersected` is `Vector() & Vector()`, and `__and__` is annotated `-> MyClass`.
        let resolved = get_resolved_symbols_at_position(session, file_symbol, file_info, 14, 0);
        assert!(
            resolved.len() == 1 && resolved[0] == my_class,
            "Vector() & Vector() should resolve to MyClass via __and__, got: {:?}",
            resolved.iter().map(|&s| session.st().name(s).to_string()).collect::<Vec<_>>()
        );
    });
}

/// Subtracting two recordsets keeps the model type, `BaseModel.__sub__` returning `Self`.
#[test]
fn test_binary_op_on_recordset_keeps_model() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let test_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/addons/module_1/models/binop_models.py")
        .sanitize();
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&test_file))
        .expect("Failed to get file symbol");
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_info = file_mgr.borrow().get_file_info(&test_file).unwrap();

    // Line 8: `difference` in `return difference`, assigned from `self` minus `others`.
    let resolved = get_resolved_symbols_at_position(&mut session, file_symbol, &file_info, 8, 15);
    let names = resolved.iter().map(|&s| session.st().name(s).to_string()).collect::<Vec<_>>();
    assert!(
        names == vec!["BinOpTestModel"],
        "self - others should resolve to BinOpTestModel via __sub__, got: {names:?}"
    );
}
