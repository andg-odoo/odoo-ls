mod setup;

use std::env;
use std::path::Path;

use lsp_types::{Documentation, ParameterLabel, SignatureHelp};
use odoo_ls_server::core::odoo::SyncOdoo;
use odoo_ls_server::features::signature_help::SignatureHelpFeature;
use odoo_ls_server::utils::PathSanitizer;

/// Request signature help in `signature_help.py` at the given position.
fn signature_help(line: u32, character: u32) -> Option<SignatureHelp> {
    let (mut odoo, config) = setup::setup::setup_server(false);
    let mut session = setup::setup::create_init_session(&mut odoo, config);
    let path = env::current_dir().unwrap()
        .join("tests/data/python/expressions/signature_help.py").sanitize();
    setup::setup::prepare_custom_entry_point(&mut session, path.as_str());
    let file_mgr = session.sync_odoo.get_file_mgr();
    let file_info = file_mgr.borrow().get_file_info(&path).unwrap();
    let file_symbol = SyncOdoo::get_symbol_of_opened_file(&mut session, Path::new(&path))
        .expect("Failed to get file symbol");
    SignatureHelpFeature::signature_help_python(&mut session, file_symbol, &file_info, line, character)
}

/// Label of the parameter pointed at by `active_parameter`, resolved through the label offsets.
fn active_parameter_label(help: &SignatureHelp) -> Option<String> {
    let signature = help.signatures.first()?;
    let parameter = signature.parameters.as_ref()?.get(signature.active_parameter? as usize)?;
    match parameter.label {
        ParameterLabel::LabelOffsets([start, end]) => Some(String::from_utf16_lossy(
            &signature.label.encode_utf16().collect::<Vec<_>>()[start as usize..end as usize]
        )),
        ParameterLabel::Simple(ref label) => Some(label.clone()),
    }
}

#[test]
fn test_signature_of_module_function() {
    // `foo()`, cursor right after the opening parenthesis
    let help = signature_help(11, 4).expect("signature help");
    assert_eq!(help.signatures.len(), 1);
    assert_eq!(help.signatures[0].label, "foo(a, b) -> Any");
    assert_eq!(active_parameter_label(&help).as_deref(), Some("a"));
}

#[test]
fn test_active_parameter_follows_the_cursor() {
    // `foo(1, 2)`: cursor on `1` then on `2`
    let on_first = signature_help(12, 4).expect("signature help");
    assert_eq!(active_parameter_label(&on_first).as_deref(), Some("a"));
    let on_second = signature_help(12, 7).expect("signature help");
    assert_eq!(active_parameter_label(&on_second).as_deref(), Some("b"));
}

#[test]
fn test_active_parameter_of_keyword_argument() {
    // `foo(b=2)`: a keyword argument highlights its own parameter, not the positional slot
    let help = signature_help(13, 6).expect("signature help");
    assert_eq!(active_parameter_label(&help).as_deref(), Some("b"));
}

#[test]
fn test_bound_method_hides_self_and_keeps_doc_string() {
    // `Spam().eggs(1)`
    let help = signature_help(14, 12).expect("signature help");
    assert_eq!(help.signatures[0].label, "eggs(a, b) -> Any");
    assert_eq!(active_parameter_label(&help).as_deref(), Some("a"));
    let Some(Documentation::MarkupContent(doc_string)) = help.signatures[0].documentation.as_ref() else {
        panic!("Expected the doc string of eggs, got {:?}", help.signatures[0].documentation);
    };
    assert_eq!(doc_string.value, "Return the first argument.");
}

#[test]
fn test_extra_arguments_land_on_vararg() {
    // `with_varargs(1, 2, 3)`: the third argument of a `(a, *rest)` call still points at `*rest`
    let help = signature_help(15, 19).expect("signature help");
    assert_eq!(help.signatures[0].label, "with_varargs(a, *rest) -> Any");
    assert_eq!(active_parameter_label(&help).as_deref(), Some("*rest"));
}

#[test]
fn test_no_signature_outside_of_a_call() {
    // `    return a`, in a function body but not in a call
    assert!(signature_help(6, 4).is_none());
}
