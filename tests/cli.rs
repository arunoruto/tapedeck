//! End-to-end checks of the `td` binary that need neither network nor yt-dlp.

use std::{fs, path::PathBuf, process::Command};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tapedeck-cli-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn td(config: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_td")).arg("--config").arg(config).args(args).output().unwrap()
}

#[test]
fn version_matches_cargo() {
    let out = Command::new(env!("CARGO_BIN_EXE_td")).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("td {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn status_runs_in_process_and_emits_json() {
    let dir = scratch("status");
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!(
            "library = {:?}\ndatabase = {:?}\nsocket = {:?}\n[listenbrainz]\nuser = \"nobody\"\n",
            dir.join("lib"),
            dir.join("td.db"),
            // Unix socket paths are capped at ~104 bytes; keep it short and unique.
            std::env::temp_dir().join(format!("td-{}.sock", std::process::id())),
        ),
    )
    .unwrap();

    let out = td(&config, &["status", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let line: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["type"], "data");
    assert_eq!(line["data"]["playlists"], 0);

    let out = td(&config, &["playlists"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "(nothing)");

    let out = td(&config, &["push", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no playlist"));

    for args in [["rm", "999"], ["retry", "999"]] {
        let out = td(&config, &args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("no track with id 999"), "{args:?}");
    }
    let out = td(&config, &["retry"]);
    assert!(out.status.success());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bad_config_is_rejected() {
    let dir = scratch("badcfg");
    let config = dir.join("config.toml");
    fs::write(&config, "library = \"/x\"\npath_template = \"{nope}\"\n[listenbrainz]\nuser = \"u\"\n").unwrap();
    let out = td(&config, &["status"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown token {nope}"));
    let _ = fs::remove_dir_all(&dir);
}
