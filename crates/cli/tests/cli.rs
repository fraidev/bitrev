use std::process::Command;

use bit_rev::identity::CLIENT_VERSION;

fn bitrev() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bitrev"))
}

#[test]
fn help_lists_every_flag() {
    let output = bitrev().arg("--help").output().expect("run bitrev --help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "--output",
        "-o",
        "--port",
        "-p",
        "--seed",
        "--no-seed",
        "--verify",
        "--config",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--version",
        "config",
    ] {
        assert!(
            help.contains(needle),
            "bitrev --help is missing {needle}:\n{help}"
        );
    }

    let config_help = bitrev()
        .args(["config", "init", "--help"])
        .output()
        .expect("run bitrev config init --help");
    assert!(config_help.status.success());
    let config_help = String::from_utf8_lossy(&config_help.stdout);
    assert!(
        config_help.contains("--force"),
        "config init help is missing --force:\n{config_help}"
    );
}

#[test]
fn version_equals_identity_constant() {
    let output = bitrev()
        .arg("--version")
        .output()
        .expect("run bitrev --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(CLIENT_VERSION),
        "--version {stdout:?} should contain {CLIENT_VERSION}"
    );
}

#[test]
fn config_init_writes_default_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let first = bitrev()
        .args(["--config", path.to_str().unwrap(), "config", "init"])
        .output()
        .expect("config init");
    assert!(
        first.status.success(),
        "config init failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let text = std::fs::read_to_string(&path).unwrap();
    let parsed: bit_rev::config::Config = toml::from_str(&text).unwrap();
    assert_eq!(parsed, bit_rev::config::Config::default());

    let second = bitrev()
        .args(["--config", path.to_str().unwrap(), "config", "init"])
        .output()
        .expect("config init overwrite");
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("--force"), "{err}");
}
