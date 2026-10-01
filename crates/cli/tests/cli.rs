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
        "--sequential",
        "--config",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--version",
        "config",
        "serve",
        "create",
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

#[test]
fn create_help_lists_flags() {
    let output = bitrev()
        .args(["create", "--help"])
        .output()
        .expect("create --help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "--output",
        "-o",
        "--piece-length",
        "--announce",
        "--private",
        "--comment",
        "--web-seed",
        "--name",
    ] {
        assert!(
            help.contains(needle),
            "create --help is missing {needle}:\n{help}"
        );
    }
}

#[test]
fn create_writes_a_torrent_that_parses() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("bundle");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("b.txt"), b"beta").unwrap();
    std::fs::write(src.join("a.txt"), b"alpha").unwrap();
    let out = dir.path().join("out.torrent");

    let run = bitrev()
        .args([
            "create",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--piece-length",
            "16384",
            "--announce",
            "http://tracker.example/announce",
            "--private",
            "--comment",
            "hello",
            "--web-seed",
            "http://cdn.example/bundle/",
        ])
        .output()
        .expect("bitrev create");
    assert!(
        run.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("out.torrent"), "{stdout}");

    let bytes = std::fs::read(&out).unwrap();
    let meta = bit_rev::file::from_bytes(&bytes).expect("parse created torrent");
    assert_eq!(meta.torrent_file.info.name, "bundle");
    assert!(meta.torrent_file.info.is_private());
    assert_eq!(meta.torrent_file.comment.as_deref(), Some("hello"));
    assert_eq!(
        meta.torrent_file.announce.as_deref(),
        Some("http://tracker.example/announce")
    );
    assert_eq!(
        meta.torrent_file.url_list.as_deref(),
        Some(["http://cdn.example/bundle/".to_string()].as_slice())
    );
    let created_by = bit_rev::identity::extension_version();
    assert_eq!(
        meta.torrent_file.created_by.as_deref(),
        Some(created_by.as_str())
    );
    let files = meta.torrent_file.info.files.expect("multi-file");
    assert_eq!(files[0].path, ["a.txt"]);
    assert_eq!(files[1].path, ["b.txt"]);

    let again = bitrev()
        .args([
            "create",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--piece-length",
            "16384",
            "--announce",
            "http://tracker.example/announce",
            "--private",
            "--comment",
            "hello",
            "--web-seed",
            "http://cdn.example/bundle/",
        ])
        .output()
        .expect("bitrev create again");
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let second = bit_rev::file::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(second.info_hash, meta.info_hash);
}

#[test]
fn create_defaults_to_a_sibling_torrent() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("payload.bin");
    std::fs::write(&src, b"xyz").unwrap();
    let run = bitrev()
        .args(["create", src.to_str().unwrap(), "--piece-length", "16384"])
        .output()
        .expect("bitrev create default");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let out = dir.path().join("payload.bin.torrent");
    assert!(out.is_file(), "missing {}", out.display());
    let meta = bit_rev::file::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(meta.torrent_file.info.name, "payload.bin");
    assert_eq!(meta.torrent_file.info.length, Some(3));
    assert!(meta.torrent_file.info.files.is_none());
}

#[test]
fn create_missing_path_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let run = bitrev()
        .args(["create", missing.to_str().unwrap()])
        .output()
        .expect("bitrev create missing");
    assert!(!run.status.success());
    let err = String::from_utf8_lossy(&run.stderr);
    assert!(err.contains("nope") || err.contains("open"), "{err}");
}
