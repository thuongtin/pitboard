//! Proves the environment is read the way Claude Code reads it, by the binary itself: the
//! combinations are unit-tested in `claude.rs`, and this checks that an empty
//! `CLAUDE_CONFIG_DIR` reaches the file opened and the slot read as unset.

use std::path::PathBuf;
use std::process::Command;

fn environment(home: &PathBuf, config_dir: Option<&str>) -> serde_json::Value {
    let out = doctor(home, config_dir).output().expect("run pitboard");
    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor --json should be valid JSON");
    assert_eq!(envelope["v"], 1, "the contract version must be present");
    assert_eq!(envelope["command"], "doctor");
    envelope["data"]["environment"].clone()
}

/// `pitboard doctor --json` with every home it reads in `home`.
fn doctor(home: &PathBuf, config_dir: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pitboard"));
    // Nothing real is read. The slot under test is the default one, so the keychain account
    // is a name nobody has and the lookup finds no item; Codex gets a home of its own; and
    // PATH holds only the system's directories, so no installed `claude` or `codex` is
    // resolved either. Claude Desktop's data folder and app are pointed at places that do
    // not exist, so neither the person's own data nor their install is read.
    command
        .args(["doctor", "--json"])
        .env("HOME", home)
        .env("USER", "pitboard-test-nobody")
        .env("PITBOARD_HOME", home.join("pitboard"))
        .env("CODEX_HOME", home.join("codex"))
        .env("PITBOARD_CLAUDE_DESKTOP_DIR", home.join("claude-desktop"))
        .env("PITBOARD_CLAUDE_DESKTOP_APP", home.join("Claude.app"))
        .env("PATH", "/usr/bin:/bin")
        .env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    match config_dir {
        Some(v) => command.env("CLAUDE_CONFIG_DIR", v),
        None => command.env_remove("CLAUDE_CONFIG_DIR"),
    };
    command
}

/// Doctor reads Claude Desktop's data folder and app too, so they are pointed into the
/// scratch home as well, whatever the environment running the suite says: neither exists
/// there, so the person's own Claude is never read.
#[test]
fn doctor_reads_no_real_claude_desktop() {
    let home = scratch("desktop");
    let command = doctor(&home, None);
    for name in ["PITBOARD_CLAUDE_DESKTOP_DIR", "PITBOARD_CLAUDE_DESKTOP_APP"] {
        let set = command
            .get_envs()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v)
            .unwrap_or_else(|| panic!("{name} is not pointed anywhere"));
        assert!(
            std::path::Path::new(set).starts_with(&home),
            "{name} is {set:?}, outside the scratch home"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pitboard-env-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join(".claude.json"), "{}").unwrap();
    p
}

#[test]
fn an_empty_config_dir_means_unset() {
    let home = scratch("empty");
    let unset = environment(&home, None);
    let empty = environment(&home, Some(""));

    assert_eq!(
        empty["config_file"], unset["config_file"],
        "an empty CLAUDE_CONFIG_DIR must resolve exactly as an unset one"
    );
    assert_eq!(
        empty["credential_service"], "Claude Code-credentials",
        "and must leave pitboard on the default credential slot"
    );
    let _ = std::fs::remove_dir_all(&home);
}
