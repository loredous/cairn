//! The command line with a config file and rig profiles, through paths that
//! touch no hardware: `tx --wav`, `rigs`, and `run` failing on its settings
//! before it opens a device.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cairn-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The binary with a private config directory and no inherited settings.
fn modem(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cairn-modem"))
        .args(args)
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env_remove("CAIRN_CONFIG")
        .env_remove("CAIRN_CALLSIGN")
        .env_remove("CAIRN_DEVICE")
        .env_remove("CAIRN_PTT")
        .output()
        .expect("run cairn-modem")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn source_in_manifest(path: &Path) -> String {
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    m["frames"][0]["header"]["source"].as_str().unwrap_or_default().to_string()
}

#[test]
fn config_values_apply_and_flags_override_them() {
    let d = tmp("override");
    std::fs::create_dir_all(d.join("xdg/cairn")).unwrap();
    std::fs::write(d.join("xdg/cairn/cairn.toml"), "[station]\ncallsign = \"N0CFG-1\"\n").unwrap();
    // The default path ($XDG_CONFIG_HOME/cairn/cairn.toml).
    let o = modem(&d, &["tx", "--wav", "a.wav", "--message", "hi"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("settings from config"));
    assert_eq!(source_in_manifest(&d.join("a.json")), "N0CFG-1");
    // A flag wins.
    let o = modem(&d, &["tx", "--wav", "b.wav", "--message", "hi", "--callsign", "N0CLI-2"]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(source_in_manifest(&d.join("b.json")), "N0CLI-2");
    // --no-config ignores the file.
    let o = modem(&d, &["--no-config", "tx", "--wav", "c.wav", "--message", "hi"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("give --callsign"), "{}", text(&o));
    // An explicit --config must exist; a broken one is an error naming the key.
    let o = modem(&d, &["--config", "missing.toml", "tx", "--wav", "c.wav"]);
    assert!(!o.status.success() && text(&o).contains("missing.toml"));
    std::fs::write(d.join("bad.toml"), "[timing]\ntxdelay = 300\n").unwrap();
    let o = modem(&d, &["--config", "bad.toml", "tx", "--wav", "c.wav"]);
    assert!(!o.status.success() && text(&o).contains("txdelay"), "{}", text(&o));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn rigs_lists_builtin_and_user_profiles() {
    let d = tmp("rigs");
    let o = modem(&d, &["rigs"]);
    assert!(o.status.success());
    let t = text(&o);
    for n in ["aioc-ht", "digirig", "signalink", "cm108-gpio"] {
        assert!(t.contains(n), "{t}");
    }
    let o = modem(&d, &["rigs", "aioc-ht"]);
    assert!(text(&o).contains("status = \"measured\"") && text(&o).contains("one pair"));
    // A user profile replaces the built-in one of the same name.
    std::fs::create_dir_all(d.join("xdg/cairn/rigs")).unwrap();
    std::fs::write(
        d.join("xdg/cairn/rigs/digirig.toml"),
        "[profile]\nname = \"digirig\"\ndescription = \"my digirig\"\nstatus = \"measured\"\n[timing]\ntxdelay_ms = 120.0\n",
    )
    .unwrap();
    let t = text(&modem(&d, &["rigs"]));
    assert!(t.contains("my digirig") && t.contains("user"), "{t}");
    let o = modem(&d, &["--rig", "nonesuch", "tx", "--wav", "x.wav", "--callsign", "N0X"]);
    assert!(!o.status.success() && text(&o).contains("no rig profile"), "{}", text(&o));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn run_rejects_bad_unattended_settings_before_opening_anything() {
    let d = tmp("unattended");
    let o = modem(&d, &["--no-config", "run", "--callsign", "N0X-1", "--unattended", "--allow", "N0*"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unattended.allow"), "{}", text(&o));
    let o = modem(&d, &["--no-config", "run", "--callsign", "N0X-1", "--unattended", "--no-watchdog"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("needs the watchdog"), "{}", text(&o));
    std::fs::write(d.join("u.toml"), "[unattended]\nenabled = true\nmax_session_min = 9999\n").unwrap();
    let o = modem(&d, &["--config", "u.toml", "run", "--callsign", "N0X-1"]);
    assert!(!o.status.success() && text(&o).contains("max_session_min"), "{}", text(&o));
    std::fs::remove_dir_all(&d).unwrap();
}
