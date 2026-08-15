#[test]
fn catalog_million_main_syncs_the_catalog() {
    temp_env::with_var("PERYX_CATALOG_PROJECTS", Some("3"), super::main).unwrap();
}
