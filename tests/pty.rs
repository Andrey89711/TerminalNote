//! Real terminal tests: no mock backend and no silent skip if PTY setup fails.
#![cfg(any(windows, unix))]

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

const TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

struct TerminalSession {
    child: Box<dyn Child + Send + Sync>,
    writer: Option<Box<dyn Write + Send>>,
    receiver: Receiver<Vec<u8>>,
    output: Vec<u8>,
    cursor_queries_answered: usize,
    master: Option<Box<dyn MasterPty + Send>>,
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl TerminalSession {
    fn spawn(root: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("native PTY must be available");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_note"));
        command.arg(root);
        command.arg("--config-file");
        command.arg(root.join(".test-config/config.toml"));
        command.env("TERM", "xterm-256color");
        command.env("TERMINALNOTE_CONFIG", root.join(".test-config/config.toml"));
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn note in PTY");
        // Install cleanup before any further fallible operations.
        // The reader enforces a total byte cap rather than blocking on a full
        // channel: ConPTY shutdown must always be able to drain its output.
        let (sender, receiver) = mpsc::channel();
        let mut session = Self {
            child,
            writer: Some(writer),
            receiver,
            output: Vec::new(),
            cursor_queries_answered: 0,
            master: Some(pair.master),
            reader_thread: None,
        };
        drop(pair.slave);
        session.reader_thread = Some(thread::spawn(move || {
            let mut buffer = [0; 4096];
            let mut total = 0;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        total += count;
                        if sender.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                        if total > OUTPUT_LIMIT {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Unix PTYs may report EIO instead of EOF after slave exit.
                    Err(_) => break,
                }
            }
        }));
        session
    }

    fn send(&mut self, bytes: &[u8]) {
        let writer = self.writer.as_mut().expect("terminal writer is open");
        writer.write_all(bytes).expect("write terminal input");
        writer.flush().expect("flush terminal input");
    }

    fn receive(&mut self, timeout: Duration) {
        if let Ok(bytes) = self.receiver.recv_timeout(timeout) {
            assert!(
                self.output.len() + bytes.len() <= OUTPUT_LIMIT,
                "PTY output exceeded bounded capture"
            );
            self.output.extend(bytes);
            // ConPTY (and some Unix backends) queries the initial cursor position.
            // Count complete queries over the accumulated stream to handle splits.
            let queries = self
                .output
                .windows(4)
                .filter(|part| *part == b"\x1b[6n")
                .count();
            while self.cursor_queries_answered < queries {
                self.send(b"\x1b[1;1R");
                self.cursor_queries_answered += 1;
            }
        }
    }

    fn expect_since(&mut self, start: usize, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let visible = visible_text(&self.output[start..]);
            if contains_text(&visible, needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for {needle:?}. Terminal output:\n{visible}"
            );
            if let Some(status) = self.child.try_wait().expect("poll child") {
                // Collect any final output already queued before reporting failure.
                self.receive(Duration::from_millis(50));
                let visible = visible_text(&self.output[start..]);
                assert!(
                    contains_text(&visible, needle),
                    "Child exited ({status}) waiting for {needle:?}:\n{visible}"
                );
                return;
            }
            self.receive(Duration::from_millis(50));
        }
    }

    fn action(&mut self, input: &[u8], expected: &str) {
        let start = self.output.len();
        self.send(input);
        self.expect_since(start, expected);
    }

    fn quit(&mut self) {
        self.send(b"q");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child exit") {
                assert!(status.success(), "note exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "note did not exit after q");
            self.receive(Duration::from_millis(25));
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let mut exited = matches!(self.child.try_wait(), Ok(Some(_)));
        if !exited {
            let _ = self.child.kill();
            // Never use an unbounded wait or join on the blocking reader thread.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        // wait is only called after nonblocking confirmation of exit.
        if exited {
            let _ = self.child.wait();
        }
        drop(self.writer.take());
        drop(self.master.take());
        let mut reader_finished = true;
        if let Some(reader) = self.reader_thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            reader_finished = reader.is_finished();
            if reader_finished {
                let _ = reader.join();
            }
        }
        if !exited || !reader_finished {
            if thread::panicking() {
                eprintln!(
                    "PTY cleanup failed: child exited={exited}, reader finished={reader_finished}"
                );
            } else {
                panic!(
                    "PTY cleanup failed: child exited={exited}, reader finished={reader_finished}"
                );
            }
        }
    }
}

fn contains_text(visible: &str, needle: &str) -> bool {
    // Diff renderers can advance the cursor over spaces rather than emit them.
    let compact = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    compact(visible).contains(&compact(needle))
}

// Remove terminal control sequences for assertions, retaining UTF-8 text. This
// is deliberately not a screen emulator; assertions concern freshly emitted text.
fn visible_text(bytes: &[u8]) -> String {
    let mut plain = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if bytes.get(index) == Some(&b'[') {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            } else if bytes.get(index) == Some(&b']') {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 7 {
                        index += 1;
                        break;
                    }
                    if bytes[index..].starts_with(b"\x1b\\") {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            } else {
                index = (index + 1).min(bytes.len());
            }
        } else {
            plain.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&plain).into_owned()
}

#[test]
fn real_terminal_preview_links_utf8_edit_save_and_quit() {
    let root = tempdir().unwrap();
    let original = "# FIRST_PREVIEW_MARKER\n\n[[Second]]\n";
    fs::write(root.path().join("First.md"), original).unwrap();
    fs::write(root.path().join("Second.md"), "SECOND_PREVIEW_MARKER\n").unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "FIRST_PREVIEW_MARKER");
    terminal.expect_since(0, "First.md");
    terminal.expect_since(0, "Second.md");
    terminal.action(b"l", "Internal links");
    terminal.action(b"\r", "SECOND_PREVIEW_MARKER");
    // Home selects the tree root; Down selects First again without Alt+Left.
    terminal.action(b"\x1b[H\x1b[B", "FIRST_PREVIEW_MARKER");
    terminal.action(b"\r", "Editing");
    let pasted = "PTY saved Кириллица ";
    terminal.action(format!("\x1b[200~{pasted}\x1b[201~").as_bytes(), "unsaved");
    terminal.action(b"\x13", "Note saved");
    assert_eq!(
        fs::read_to_string(root.path().join("First.md")).unwrap(),
        format!("{pasted}{original}")
    );
    terminal.action(b"\x1b", "Preview");
    terminal.quit();
    assert_eq!(
        fs::read_to_string(root.path().join("First.md")).unwrap(),
        format!("{pasted}{original}")
    );
}

#[test]
fn real_terminal_ctrl_f_finds_a_match_only_in_the_note_body() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("First.md"), "INITIAL_PREVIEW\n").unwrap();
    let target = root.path().join("Second.md");
    let original = "SECOND_PREVIEW\n\nUniqueBodyToken\n";
    fs::write(&target, original).unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "INITIAL_PREVIEW");
    terminal.action(b"\x06", "Search names and note text");
    terminal.send(b"\x1b[200~uniquebodytoken\x1b[201~");
    terminal.action(b"\r", "SECOND_PREVIEW");
    terminal.quit();
    assert_eq!(fs::read_to_string(target).unwrap(), original);
}

#[test]
fn real_terminal_moves_selected_search_results_from_the_palette() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("Archive")).unwrap();
    fs::write(
        root.path().join("First.md"),
        "INITIAL_BULK_PREVIEW\n\nBatchToken\n",
    )
    .unwrap();
    fs::write(root.path().join("Second.txt"), "BatchToken\n").unwrap();
    fs::write(root.path().join("Other.md"), "keep\n").unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "INITIAL_BULK_PREVIEW");
    terminal.action(b"\x06", "Search names and note text");
    terminal.send(b"\x1b[200~batchtoken\x1b[201~");
    terminal.action(b"\r", "Notes");
    terminal.action(b"\x01", "Marked: 2");
    terminal.action(b"\x10", "Commands");
    terminal.send(b"\x1b[200~move marked\x1b[201~");
    terminal.action(b"\r", "Notes to move");
    terminal.send(b"\x1b[200~Archive\x1b[201~");
    terminal.action(b"\r", "Completed: 2");
    terminal.quit();
    assert!(root.path().join("Archive/First.md").is_file());
    assert!(root.path().join("Archive/Second.txt").is_file());
    assert!(!root.path().join("First.md").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("Other.md")).unwrap(),
        "keep\n"
    );
}

#[test]
fn real_terminal_external_conflict_retains_edits_and_confirms_discard() {
    let root = tempdir().unwrap();
    let path = root.path().join("First.md");
    fs::write(&path, "CONFLICT_PREVIEW_MARKER\n").unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "CONFLICT_PREVIEW_MARKER");
    terminal.action(b"\r", "Editing");
    terminal.action(b"\x1b[200~local edits \x1b[201~", "unsaved");
    fs::write(&path, "external writer\n").unwrap();
    terminal.action(b"\x13", "Save failed");
    assert_eq!(fs::read_to_string(&path).unwrap(), "external writer\n");
    terminal.action(b"\x1b", "You have unsaved changes");
    // Cancelling only erases the popup; the editor's first line may not be
    // re-emitted by the diff renderer. A second Esc must confirm again, proving
    // cancellation retained the dirty editor rather than discarding its text.
    terminal.send(b"c");
    terminal.action(b"\x1b", "You have unsaved changes");
    terminal.action(b"d", "external writer");
    terminal.quit();
    assert_eq!(fs::read_to_string(&path).unwrap(), "external writer\n");
}

#[test]
fn real_terminal_palette_saves_without_inserting_commands_and_tree_opens_nested_note() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("Work")).unwrap();
    fs::write(root.path().join("First.md"), "ROOT_NOTE_MARKER\n").unwrap();
    fs::write(root.path().join("Work/Inbox.md"), "NESTED_NOTE_MARKER\n").unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "ROOT_NOTE_MARKER");
    terminal.action(b"\x1b[H", "Folder selected"); // Root.
    terminal.send(b"\x1b[C"); // First child: Work.
    terminal.send(b"\x1b[C"); // Expand Work.
    terminal.action(b"\x1b[C", "NESTED_NOTE_MARKER"); // First child: Inbox.
    terminal.action(b"\r", "Editing");
    terminal.action(b"\x1b[200~PALETTE_EDIT \x1b[201~", "unsaved");
    terminal.action(b"\x10", "Commands");
    terminal.send(b"\x1b[200~save\x1b[201~");
    terminal.action(b"\r", "Note saved");
    assert_eq!(
        fs::read_to_string(root.path().join("Work/Inbox.md")).unwrap(),
        "PALETTE_EDIT NESTED_NOTE_MARKER\n"
    );
    terminal.action(b"\x1b", "Preview");
    terminal.quit();
}

#[test]
fn real_terminal_language_choice_is_persisted_and_loaded_on_restart() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("First.md"), "User text stays unchanged.\n").unwrap();
    {
        let mut terminal = TerminalSession::spawn(root.path());
        terminal.expect_since(0, "Explorer");
        terminal.action(b"\x10", "Commands");
        terminal.send(b"\x1b[200~language\x1b[201~");
        terminal.action(b"\r", "Interface language");
        terminal.send(b"\x1b[B");
        terminal.action(b"\r", "Язык изменён");
        terminal.quit();
    }
    let config_path = root.path().join(".test-config/config.toml");
    assert!(
        config_path.is_file(),
        "Language config missing; temporary root entries: {:?}",
        fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains("language = \"ru\""));
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "Проводник");
    terminal.expect_since(0, "User text stays unchanged.");
    terminal.quit();
    assert_eq!(
        fs::read_to_string(root.path().join("First.md")).unwrap(),
        "User text stays unchanged.\n"
    );
}

#[test]
fn real_terminal_update_consent_can_be_cancelled_without_a_network_request() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("First.md"), "UPDATE_CONSENT_NOTE\n").unwrap();
    let mut terminal = TerminalSession::spawn(root.path());
    terminal.expect_since(0, "UPDATE_CONSENT_NOTE");
    terminal.action(b"\x10", "Commands");
    terminal.send(b"\x1b[200~check updates\x1b[201~");
    terminal.action(b"\r", "Send a request to api.github.com?");
    terminal.action(b"\x1b", "Ctrl+P actions"); // Wait for dismissal before sending another key.
    terminal.quit();
    assert_eq!(
        fs::read_to_string(root.path().join("First.md")).unwrap(),
        "UPDATE_CONSENT_NOTE\n"
    );
}
