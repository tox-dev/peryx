use peryx_driver::serving::PolicyDriver as _;

use super::PrefetchConfig;
use crate::serving::PypiServing;

#[test]
fn prefetch_config_compiles_pypi_options() {
    let config = PrefetchConfig::from_table(
        &toml::from_str(
            r#"
mode = "metadata-only"
packages = ["requests>=2,<3"]
requirements = ["requirements.txt"]
include_wheels = false
include_sdists = true
python_tags = ["py3"]
abi_tags = ["none"]
platform_tags = ["any"]
max_file_size_bytes = 1048576
"#,
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(config.packages, ["requests>=2,<3"]);
    assert_eq!(config.requirements, [std::path::PathBuf::from("requirements.txt")]);
    assert!(!config.include_wheels);
    assert!(config.include_sdists);
    assert_eq!(config.python_tags, ["py3"]);
    assert_eq!(config.abi_tags, ["none"]);
    assert_eq!(config.platform_tags, ["any"]);
    assert_eq!(config.max_file_size_bytes, Some(1_048_576));
}

#[test]
fn policy_driver_compiles_pypi_policy_keys() {
    let capabilities = PypiServing
        .compile_policy(
            &toml::from_str(
                r#"
allow_versions = ">=1,<2"
allow_package_types = ["wheel"]
block_package_types = ["sdist"]
allow_wheel_pythons = ["py3"]
block_wheel_pythons = ["py2"]
allow_wheel_platforms = ["any"]
block_wheel_platforms = ["win_amd64"]
"#,
            )
            .unwrap(),
        )
        .unwrap();

    assert!(!capabilities.is_empty());
}
