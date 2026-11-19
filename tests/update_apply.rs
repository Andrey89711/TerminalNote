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
fn wait_for(path: &Path, child: &mut Child, log: &Path) {
    let until = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "helper exited before handoff: {}",
            fs::read_to_string(log).unwrap_or_default()
        );
        assert!(Instant::now() < until, "helper timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_fixture_owner(path: &Path) {
    // Elevated hosted runners can default new files to the Administrators group.
    // Give this disposable fixture the same ownership as a per-user installation.
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT},
            EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let paths = std::iter::once(path.to_owned()).chain(
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path()),
    );
    unsafe {
        let mut token = ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let mut size = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let ok = GetTokenInformation(token, TokenUser, data.as_mut_ptr().cast(), size, &mut size);
        CloseHandle(token);
        assert_ne!(ok, 0);
        let sid = (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        for file in paths {
            let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut owner = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            assert_eq!(
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut descriptor
                ),
                0
            );
            let same = !owner.is_null() && EqualSid(sid, owner) != 0;
            LocalFree(descriptor);
            if !same {
                assert_eq!(
                    SetNamedSecurityInfoW(
                        wide.as_ptr(),
                        SE_FILE_OBJECT,
                        OWNER_SECURITY_INFORMATION,
                        sid,
                        ptr::null_mut(),
                        ptr::null(),
                        ptr::null()
                    ),
                    0,
                    "fixture ownership"
                );
            }
        }
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
        set_fixture_owner(program.path());
        set_fixture_owner(stage.path());
        let log = stage.path().join("fixture-helper.log");
        let mut helper = Command::new(stage.path().join("helper.exe"))
            .arg("--apply-terminalnote-update")
            .current_dir(stage.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        wait_for(&stage.path().join("ready"), &mut helper, &log);
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
