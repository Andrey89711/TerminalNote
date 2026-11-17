use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::tempdir;

fn note(root: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_note"))
        .args(flags)
        .arg(root)
        .arg("--config-file")
        .arg(root.join(".test-config/config.toml"))
        .env("TERMINALNOTE_CONFIG", root.join(".test-config/config.toml"))
        .stdin(Stdio::null())
        .output()
        .expect("run note binary")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn no_escapes(output: &Output) {
    assert!(!output.stdout.contains(&0x1b), "{}", output_text(output));
    assert!(!output.stderr.contains(&0x1b), "{}", output_text(output));
}

#[test]
fn help_and_version_work_without_a_terminal() {
    for flag in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_note"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", output_text(&output));
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        if flag == "--help" {
            for option in ["--list", "--check", "--init", "--set-default"] {
                assert!(stdout.contains(option), "{stdout}");
            }
        } else {
            assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
        }
        no_escapes(&output);
    }
}

#[test]
fn list_prints_only_sorted_relative_note_paths() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    for path in ["First.md", "Second.TXT", "nested/Third.md", "ignored.bin"] {
        fs::write(root.path().join(path), "demo").unwrap();
    }
    let output = note(root.path(), &["--list"]);
    assert!(output.status.success(), "{}", output_text(&output));
    let paths: Vec<_> = String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| line.replace('\\', "/"))
        .collect();
    assert_eq!(paths, ["First.md", "Second.TXT", "nested/Third.md"]);
    assert!(output.stderr.is_empty(), "{}", output_text(&output));
    no_escapes(&output);
}

#[test]
fn check_accepts_valid_wiki_and_markdown_links() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(
        root.path().join("First.md"),
        "# First\n[[Second]]\n[third](nested/Third.md#heading)\n[web](https://example.com)\n",
    )
    .unwrap();
    fs::write(root.path().join("Second.txt"), "Привет, мир\n").unwrap();
    fs::write(
        root.path().join("nested/Third.md"),
        "# heading\n[[../First]]\n",
    )
    .unwrap();
    let output = note(root.path(), &["--check"]);
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Checked 3 notes"));
    no_escapes(&output);
}

#[test]
fn check_fails_on_broken_links_and_invalid_utf8() {
    for (contents, diagnostic) in [
        (b"[[Missing]]".as_slice(), "Missing"),
        (&[0xff, 0xfe], "UTF-8"),
    ] {
        let root = tempdir().unwrap();
        fs::write(root.path().join("Broken.md"), contents).unwrap();
        let output = note(root.path(), &["--check"]);
        assert!(!output.status.success(), "{}", output_text(&output));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("Broken.md") && error.contains(diagnostic),
            "{error}"
        );
        no_escapes(&output);
    }
}

#[test]
fn missing_paths_are_never_implicitly_created() {
    let root = tempdir().unwrap();
    for flags in [&["--list"][..], &["--check"][..], &[][..]] {
        let missing = root.path().join("missing").join("notes");
        let output = note(&missing, flags);
        assert!(!output.status.success(), "{}", output_text(&output));
        assert!(!root.path().join("missing").exists());
        no_escapes(&output);
    }
}

#[test]
fn non_tty_tui_and_init_fail_before_creating_anything() {
    let root = tempdir().unwrap();
    for flags in [&[][..], &["--init"][..]] {
        let missing = root.path().join("must-not-create");
        let output = note(&missing, flags);
        assert!(!output.status.success(), "{}", output_text(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("terminal"),
            "{}",
            output_text(&output)
        );
        assert!(!missing.exists());
        assert!(output.stdout.is_empty());
        no_escapes(&output);
    }
    let output = note(root.path(), &[]);
    assert!(!output.status.success());
    no_escapes(&output);
}

#[test]
fn conflicting_and_unknown_options_fail_without_side_effects() {
    let root = tempdir().unwrap();
    let missing = root.path().join("missing");
    for flags in [
        vec!["--list", "--check"],
        vec!["--init", "--list"],
        vec!["--init", "--check"],
        vec!["--not-an-option"],
    ] {
        let output = note(&missing, &flags);
        assert!(!output.status.success(), "{}", output_text(&output));
        assert!(!missing.exists());
        no_escapes(&output);
    }
}
