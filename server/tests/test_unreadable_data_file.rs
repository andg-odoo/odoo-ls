mod setup;
mod test_utils;

/// An undecodable manifest data file is skipped instead of panicking the whole server.
#[test]
fn test_module_with_an_unreadable_data_file_still_builds() {
    let (mut odoo, config) = setup::setup::setup_server(true);
    let session = setup::setup::create_init_session(&mut odoo, config);

    assert!(
        session.sync_odoo.modules.keys().any(|name| name.as_str() == "module_broken_encoding"),
        "Expected module_broken_encoding to be loaded despite its unreadable data file"
    );
}
