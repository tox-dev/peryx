use super::EcosystemInstaller;

struct Installer;

impl EcosystemInstaller<Vec<&'static str>> for Installer {
    fn register_driver(&self, state: &mut Vec<&'static str>) {
        state.push("registered");
    }
}

#[test]
fn test_default_install_registers_the_driver() {
    let mut state = Vec::new();

    Installer.install(&mut state);

    assert_eq!(state, ["registered"]);
}
