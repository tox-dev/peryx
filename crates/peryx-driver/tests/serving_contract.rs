use peryx_core::Ecosystem;
use peryx_driver::serving::CompiledEcosystemSettings;

#[test]
fn compiled_settings_expose_only_their_ecosystem_and_typed_value() {
    let settings = CompiledEcosystemSettings::new(Ecosystem::new("example"), 42_u64);

    assert_eq!(settings.ecosystem(), Ecosystem::new("example"));
    assert_eq!(settings.value::<u64>(), Some(&42));
    assert_eq!(settings.value::<String>(), None);
    assert_eq!(
        format!("{settings:?}"),
        "CompiledEcosystemSettings { ecosystem: Ecosystem(\"example\"), .. }"
    );
}
