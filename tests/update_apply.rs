//! Real Windows helper handoff, replacement, rollback and relaunch; entirely local fixtures.
#![cfg(windows)]
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Parent(Child);
impl Drop for Parent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}
fn wait_for(path: &Path, child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "helper exited before handoff"
        );
        assert!(Instant::now() < until, "helper timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn native_helper_applies_and_relaunches_with_original_arguments_and_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let fixture_source = root.path().join("fixture.rs");
    fs::write(&fixture_source, r#"
        fn main() {
            if std::env::args().nth(1).as_deref() == Some("--version") {
                println!("note 99.0.0");
            } else {
                std::fs::write("relaunched.txt", std::env::args().skip(1).collect::<Vec<_>>().join("|")).unwrap();
            }
        }
    "#).unwrap();
    let fixture = root.path().join("fixture.exe");
    assert!(Command::new("rustc")
        .arg("--crate-name")
        .arg("update_fixture")
        .arg(&fixture_source)
        .arg("-o")
        .arg(&fixture)
        .status()
        .unwrap()
        .success());
    for success in [true, false] {
        let program = tempfile::tempdir().unwrap();
        let stage = tempfile::Builder::new()
            .prefix("terminalnote-update-")
            .tempdir_in(dirs::data_local_dir().unwrap())
            .unwrap();
        let target = program.path().join("note.exe");
        fs::copy(env!("CARGO_BIN_EXE_note"), &target).unwrap();
        let original = hash(&target);
        fs::write(program.path().join("unknown.txt"), b"keep me").unwrap();
        fs::write(
            program.path().join(".terminalnote-install.json"),
            br#"{"product":"TerminalNote","schema":1,"version":"0.1.0","unknown":"keep"}"#,
        )
        .unwrap();
        fs::copy(env!("CARGO_BIN_EXE_note"), stage.path().join("helper.exe")).unwrap();
        fs::copy(
            if success { &fixture } else { &target },
            stage.path().join("payload.exe"),
        )
        .unwrap();
        // A suspended real note process lets us test exact PID/image binding without
        // opening a terminal or reading any real configuration/notes.
        let mut parent = Parent(
            Command::new(&target)
                .arg("--version")
                .creation_flags(0x00000004)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let plan = serde_json::json!({
            "target": target, "original_hash": original,
            "payload_hash": hash(&stage.path().join("payload.exe")),
            "version": "99.0.0", "pid": parent.0.id(),
            "args": (["relative-notes", "--config-file", "isolated-test.toml"].into_iter().map(std::ffi::OsString::from).collect::<Vec<_>>()),
            "cwd": program.path(),
        });
        fs::write(
            stage.path().join("plan.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        let mut helper = Command::new(stage.path().join("helper.exe"))
            .arg("--apply-terminalnote-update")
            .current_dir(stage.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&stage.path().join("ready"), &mut helper);
        parent.0.kill().unwrap();
        parent.0.wait().unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = helper.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "helper did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.success(), success);
        if success {
            let path = program.path().join("relaunched.txt");
            while !path.exists() {
                assert!(Instant::now() < until, "relaunch did not complete");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "relative-notes|--config-file|isolated-test.toml"
            );
            assert_eq!(hash(&target), hash(&fixture));
        } else {
            assert_eq!(
                hash(&target),
                original,
                "version mismatch must leave original intact"
            );
        }
        assert_eq!(
            fs::read(program.path().join("unknown.txt")).unwrap(),
            b"keep me"
        );
        assert!(
            fs::read_to_string(program.path().join(".terminalnote-install.json"))
                .unwrap()
                .contains("\"unknown\":\"keep\"")
        );
    }
}
