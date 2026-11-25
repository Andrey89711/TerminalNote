//! Local note storage with bounded reads and optimistic, atomic replacement.
//!
//! The selected root is canonicalized (and may itself have been a symlink).
//! Below it, symlinks and Windows reparse points are never intentionally followed.
//! Component checks are best effort: portable filesystem APIs cannot eliminate
//! symlink substitution races, or a concurrent write between the final comparison
//! and rename. This is not a security boundary against a hostile local process.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

pub const MAX_NOTE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_ENTRIES: usize = 10_000;
const BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone)]
pub struct NoteEntry {
    pub path: PathBuf,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct Listing {
    pub notes: Vec<NoteEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NoteStore {
    root: PathBuf,
}

#[derive(Debug)]
pub struct Document {
    pub path: PathBuf,
    pub text: String,
    revision: Vec<u8>,
    revision_path: PathBuf,
    bom: bool,
    crlf: bool,
}

impl NoteStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .context("Cannot open notes root")?;
        if !root.is_dir() {
            bail!("Notes root is not a directory: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lists md/txt files, including dotted filenames and ordinary dotted dirs.
    /// Only .git, .hg, .svn, target and node_modules directories are excluded
    /// (case-insensitively). Up to 16 directory levels and 10,000 total entries
    /// are enumerated globally, including directories, ignored files, links,
    /// excluded directories and enumeration errors. The budget is applied before
    /// buffering/sorting; results are sorted, but a truncated subset depends on
    /// filesystem enumeration order. Reaching the budget warns conservatively,
    /// even if the tree happens to contain exactly that many entries.
    /// Inaccessible/invalid entries also warn; links are silently skipped.
    pub fn list(&self) -> Result<Listing> {
        self.list_with_entry_limit(MAX_ENTRIES)
    }

    fn list_with_entry_limit(&self, limit: usize) -> Result<Listing> {
        self.check_root()?;
        let mut listing = Listing::default();
        let mut remaining = limit;
        self.walk(Path::new(""), 0, &mut remaining, &mut listing)?;
        if remaining == 0 {
            listing.warnings.push(format!(
                "Entry limit of {limit} reached; listing may be incomplete"
            ));
        }
        listing.notes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(listing)
    }

    pub fn load(&self, relative: &Path) -> Result<Document> {
        let path = self.resolve(relative, false)?;
        let bytes = read_bounded(&path)?;
        let bom = bytes.starts_with(BOM);
        let body = if bom { &bytes[BOM.len()..] } else { &bytes };
        let raw = std::str::from_utf8(body).context("Note is not valid UTF-8")?;
        // Mixed newline files normalize to LF; uniformly CRLF files retain CRLF.
        let crlf = raw.contains("\r\n")
            && raw
                .as_bytes()
                .iter()
                .enumerate()
                .all(|(i, &b)| b != b'\n' || (i > 0 && raw.as_bytes()[i - 1] == b'\r'));
        Ok(Document {
            path: relative.to_owned(),
            text: raw.replace("\r\n", "\n"),
            revision: bytes,
            revision_path: relative.to_owned(),
            bom,
            crlf,
        })
    }

    pub fn save(&self, document: &mut Document, text: &str) -> Result<()> {
        if document.path != document.revision_path {
            bail!("Document path changed; load or create the destination instead");
        }
        let normalized = text.replace("\r\n", "\n");
        let body = if document.crlf {
            normalized.replace('\n', "\r\n")
        } else {
            normalized.clone()
        };
        let mut bytes = Vec::with_capacity(body.len() + if document.bom { 3 } else { 0 });
        if document.bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(body.as_bytes());
        if bytes.len() > MAX_NOTE_BYTES {
            bail!("Note exceeds the 2 MiB limit (including BOM and encoded newlines)");
        }
        let path = self
            .resolve(&document.path, false)
            .context("Cannot save: note was deleted, moved, or is no longer accessible")?;
        let current = read_bounded(&path)
            .context("Cannot save: note was deleted, changed, or is no longer readable")?;
        if current != document.revision {
            bail!("Cannot save: note changed on disk; reload it before saving");
        }
        let permissions = fs::metadata(&path)?.permissions();
        let mut temporary = NamedTempFile::new_in(path.parent().context("Missing parent")?)
            .context("Cannot create temporary note file")?;
        temporary.write_all(&bytes).context("Cannot write note")?;
        temporary
            .as_file()
            .set_permissions(permissions)
            .context("Cannot preserve note permissions")?;
        temporary
            .as_file()
            .sync_all()
            .context("Cannot flush note")?;

        // Revalidate after writing the temporary file, as close to rename as possible.
        let path = self
            .resolve(&document.path, false)
            .context("Cannot save: note was deleted or its path changed")?;
        let current = read_bounded(&path)
            .context("Cannot save: note was deleted, changed, or is no longer readable")?;
        if current != document.revision {
            bail!("Cannot save: note changed on disk; reload it before saving");
        }
        temporary
            .persist(&path)
            .map_err(|e| e.error)
            .context("Cannot atomically replace note")?;
        document.text = normalized;
        document.revision = bytes;
        Ok(())
    }

    /// Creates an empty note without replacing any existing entry. The caller
    /// supplies the extension; parent directories must already exist. Excluded
    /// parent directories and more than 16 parent levels are rejected so new
    /// notes remain in the listable namespace (subject to listing's entry cap).
    pub fn create(&self, relative: &Path) -> Result<Document> {
        validate_relative(relative)?;
        let parent = relative.parent().context("Missing note parent")?;
        if parent.components().count() > MAX_DEPTH {
            bail!("Cannot create note: parent depth exceeds the listing limit of {MAX_DEPTH}");
        }
        if parent
            .components()
            .any(|part| excluded_directory(part.as_os_str()))
        {
            bail!("Cannot create note inside a directory excluded from listing (.git, .hg, .svn, target, node_modules)");
        }
        let path = self.resolve(relative, true)?;
        let temporary = NamedTempFile::new_in(path.parent().context("Missing parent")?)
            .context("Cannot create temporary note file")?;
        temporary.as_file().sync_all()?;
        // Check parent components again; persist_noclobber handles final collisions.
        self.resolve(relative, true)?;
        temporary
            .persist_noclobber(&path)
            .map_err(|e| e.error)
            .context("Cannot create note (the destination may already exist)")?;
        Ok(Document {
            path: relative.to_owned(),
            text: String::new(),
            revision: Vec::new(),
            revision_path: relative.to_owned(),
            bom: false,
            crlf: false,
        })
    }

    fn check_root(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.root).context("Cannot access notes root")?;
        if is_link(&metadata) || !metadata.is_dir() {
            bail!("Notes root is no longer a regular directory");
        }
        Ok(())
    }

    fn resolve(&self, relative: &Path, allow_missing: bool) -> Result<PathBuf> {
        validate_relative(relative)?;
        if !note_extension(relative) {
            bail!("Only .md and .txt notes are supported");
        }
        self.check_root()?;
        let mut path = self.root.clone();
        let count = relative.components().count();
        for (index, component) in relative.components().enumerate() {
            path.push(component.as_os_str());
            let last = index + 1 == count;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error)
                    if last && allow_missing && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(path);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("Cannot access {}", path.display()))
                }
            };
            if is_link(&metadata) {
                bail!(
                    "Symlinks and reparse points are not allowed: {}",
                    path.display()
                );
            }
            if last && !metadata.is_file() {
                bail!("Note is not a regular file: {}", path.display());
            }
            if !last && !metadata.is_dir() {
                bail!("Note parent is not a directory: {}", path.display());
            }
        }
        Ok(path)
    }

    fn walk(
        &self,
        relative: &Path,
        depth: usize,
        remaining: &mut usize,
        listing: &mut Listing,
    ) -> Result<()> {
        if *remaining == 0 {
            return Ok(());
        }
        let directory = self.root.join(relative);
        // Recheck components before descending; directory entries are not trusted.
        let mut checked = self.root.clone();
        for component in relative.components() {
            checked.push(component.as_os_str());
            let metadata = fs::symlink_metadata(&checked)?;
            if is_link(&metadata) || !metadata.is_dir() {
                bail!("Directory changed or is a link: {}", checked.display());
            }
        }
        let entries = fs::read_dir(&directory)
            .with_context(|| format!("Cannot list {}", directory.display()))?;
        // Charge every yielded item once, before collecting or filtering. The
        // shared budget also bounds directory-heavy and entirely ignored trees.
        let mut buffered = Vec::new();
        for entry in entries.take(*remaining) {
            *remaining -= 1;
            match entry {
                Ok(entry) => buffered.push(entry),
                Err(error) => {
                    listing.warnings.push(format!(
                        "Cannot read entry in {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
        buffered.sort_by_key(|entry| entry.file_name());
        for entry in buffered {
            let child = relative.join(entry.file_name());
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    listing
                        .warnings
                        .push(format!("Cannot inspect {}: {error}", child.display()));
                    continue;
                }
            };
            if is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if excluded_directory(&entry.file_name()) {
                    continue;
                }
                if let Err(error) = validate_relative(&child) {
                    listing
                        .warnings
                        .push(format!("Skipping directory {}: {error}", child.display()));
                } else if depth >= MAX_DEPTH {
                    listing.warnings.push(format!(
                        "Depth limit of {MAX_DEPTH} reached at {}",
                        child.display()
                    ));
                } else if let Err(error) = self.walk(&child, depth + 1, remaining, listing) {
                    listing
                        .warnings
                        .push(format!("Skipping directory {}: {error:#}", child.display()));
                }
            } else if metadata.is_file() {
                if !note_extension(&child) {
                    continue;
                }
                if let Err(error) = validate_relative(&child) {
                    listing
                        .warnings
                        .push(format!("Skipping note {}: {error}", child.display()));
                    continue;
                }
                listing.notes.push(NoteEntry {
                    title: child
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path: child,
                });
            }
        }
        Ok(())
    }
}

fn excluded_directory(name: &OsStr) -> bool {
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        ".git" | ".hg" | ".svn" | "target" | "node_modules"
    )
}

fn note_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("txt")
        })
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("Note path must not be empty");
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            bail!("Note path must be relative without traversal components");
        };
        let name = name.to_str().context("Note path must be valid UTF-8")?;
        if name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
            || name.ends_with(['.', ' '])
        {
            bail!("Note path contains an unsafe filename");
        }
        let stem = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_uppercase();
        let device_number = |prefix: &str| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        };
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || device_number("COM")
            || device_number("LPT")
        {
            bail!("Windows device names are not allowed");
        }
    }
    Ok(())
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("Note is not a regular file");
    }
    if metadata.len() > MAX_NOTE_BYTES as u64 {
        bail!("Note exceeds the 2 MiB limit");
    }
    let mut bytes = Vec::new();
    file.take(MAX_NOTE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_NOTE_BYTES {
        bail!("Note exceeds the 2 MiB limit");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn root_must_exist_and_be_directory() {
        let dir = tempdir().unwrap();
        assert!(NoteStore::open(dir.path().join("missing")).is_err());
        fs::write(dir.path().join("file"), "").unwrap();
        assert!(NoteStore::open(dir.path().join("file")).is_err());
        assert_eq!(
            NoteStore::open(dir.path()).unwrap().root(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn unsafe_paths_and_extensions_are_rejected() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        for name in [
            "",
            "../escape.md",
            "a/../../escape.md",
            "./a.md",
            "NUL.txt",
            "COM1.md",
            "lpt9.txt",
            "a.md ",
            "a.md.",
            "a:b.md",
            "a\0.md",
            "a\n.md",
            "a.exe",
            "noextension",
            "a\\b.md",
        ] {
            assert!(store.create(Path::new(name)).is_err(), "create {name:?}");
            assert!(store.load(Path::new(name)).is_err(), "load {name:?}");
        }
        assert!(store.create(&dir.path().join("absolute.md")).is_err());
        assert!(store.create(Path::new("missing/note.md")).is_err());
    }

    #[test]
    fn listing_is_sorted_nested_and_filtered() {
        let dir = tempdir().unwrap();
        for sub in [
            "nested",
            ".ordinary",
            ".git",
            ".hg",
            ".svn",
            "target",
            "node_modules",
        ] {
            fs::create_dir(dir.path().join(sub)).unwrap();
            fs::write(dir.path().join(sub).join("inside.md"), "").unwrap();
        }
        for name in ["z.TXT", "a.Md", ".dotted.md", "ignored.rs"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let listing = NoteStore::open(dir.path()).unwrap().list().unwrap();
        let mut expected = [
            "z.TXT",
            "a.Md",
            ".dotted.md",
            ".ordinary/inside.md",
            "nested/inside.md",
        ]
        .map(PathBuf::from)
        .to_vec();
        expected.sort();
        assert_eq!(
            listing
                .notes
                .iter()
                .map(|n| n.path.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(listing.notes.iter().any(|n| n.title == "a"));
        assert!(listing.warnings.is_empty());
    }

    #[test]
    fn bom_crlf_and_utf8_roundtrip() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let path = Path::new("note.md");
        fs::write(dir.path().join(path), b"\xef\xbb\xbfhello\r\nworld\r\n").unwrap();
        let mut doc = store.load(path).unwrap();
        assert_eq!(doc.text, "hello\nworld\n");
        store.save(&mut doc, "caf\u{e9}\nnew\n").unwrap();
        assert_eq!(
            fs::read(dir.path().join(path)).unwrap(),
            "\u{feff}caf\u{e9}\r\nnew\r\n".as_bytes()
        );
        assert_eq!(doc.text, "caf\u{e9}\nnew\n");
        fs::write(dir.path().join("bad.txt"), [0xff]).unwrap();
        assert!(store.load(Path::new("bad.txt")).is_err());
        let mut fresh = store.create(Path::new("new.txt")).unwrap();
        store.save(&mut fresh, "one\ntwo").unwrap();
        assert_eq!(fs::read(dir.path().join("new.txt")).unwrap(), b"one\ntwo");
    }

    #[test]
    fn exact_bytes_conflicts_and_deletion_leave_document_unchanged() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let path = Path::new("a.md");
        fs::write(dir.path().join(path), "original").unwrap();
        let mut doc = store.load(path).unwrap();
        fs::write(dir.path().join(path), "modified").unwrap(); // same length
        assert!(store
            .save(&mut doc, "edited")
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert_eq!(doc.text, "original");
        assert_eq!(doc.revision, b"original");
        assert_eq!(fs::read(dir.path().join(path)).unwrap(), b"modified");
        fs::remove_file(dir.path().join(path)).unwrap();
        assert!(store
            .save(&mut doc, "edited")
            .unwrap_err()
            .to_string()
            .contains("deleted"));
        assert_eq!(doc.text, "original");
        assert_eq!(doc.revision, b"original");
        assert!(!dir.path().join(path).exists());
    }

    #[test]
    fn size_limits_include_encoding_and_bom() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let path = Path::new("a.md");
        let exact = vec![b'a'; MAX_NOTE_BYTES];
        fs::write(dir.path().join(path), &exact).unwrap();
        let mut doc = store.load(path).unwrap();
        store.save(&mut doc, &"b".repeat(MAX_NOTE_BYTES)).unwrap();
        assert!(store
            .save(&mut doc, &"x".repeat(MAX_NOTE_BYTES + 1))
            .is_err());
        assert_eq!(doc.text.len(), MAX_NOTE_BYTES);
        fs::write(dir.path().join(path), vec![b'a'; MAX_NOTE_BYTES + 1]).unwrap();
        assert!(store.load(path).is_err());
        fs::write(dir.path().join(path), b"\xef\xbb\xbf\r\n").unwrap();
        let mut doc = store.load(path).unwrap();
        assert!(store
            .save(&mut doc, &"a".repeat(MAX_NOTE_BYTES - 2))
            .is_err());
        assert!(store
            .save(&mut doc, &"\n".repeat(MAX_NOTE_BYTES / 2))
            .is_err());
        assert_eq!(doc.text, "\n");
        assert_eq!(doc.revision, b"\xef\xbb\xbf\r\n");
    }

    #[test]
    fn create_never_overwrites_and_path_mutation_is_not_a_rename() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let mut doc = store.create(Path::new("a.md")).unwrap();
        store.save(&mut doc, "keep me").unwrap();
        assert!(store.create(Path::new("a.md")).is_err());
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), b"keep me");
        doc.path = PathBuf::from("b.md");
        assert!(store.save(&mut doc, "no").is_err());
        assert_eq!(doc.text, "keep me");
        assert!(!dir.path().join("b.md").exists());
    }

    #[test]
    fn depth_limit_warns_and_includes_level_sixteen() {
        let dir = tempdir().unwrap();
        let mut path = dir.path().to_owned();
        for _ in 0..MAX_DEPTH {
            path.push("d");
            fs::create_dir(&path).unwrap();
        }
        fs::write(path.join("included.md"), "").unwrap();
        fs::create_dir(path.join("too-deep")).unwrap();
        fs::write(path.join("too-deep/excluded.md"), "").unwrap();
        let listing = NoteStore::open(dir.path()).unwrap().list().unwrap();
        assert_eq!(listing.notes.len(), 1);
        assert!(listing.warnings.iter().any(|w| w.contains("Depth limit")));
    }

    #[test]
    fn entry_count_is_bounded() {
        let dir = tempdir().unwrap();
        for index in 0..=MAX_ENTRIES {
            fs::write(dir.path().join(format!("{index:05}.md")), "").unwrap();
        }
        let listing = NoteStore::open(dir.path()).unwrap().list().unwrap();
        assert_eq!(listing.notes.len(), MAX_ENTRIES);
        assert_eq!(
            listing
                .warnings
                .iter()
                .filter(|w| w.contains("Entry limit"))
                .count(),
            1
        );
    }

    #[test]
    fn directory_entries_exhaust_budget_before_recursion() {
        let dir = tempdir().unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::create_dir(dir.path().join(name)).unwrap();
            fs::write(dir.path().join(name).join("inside.md"), "").unwrap();
        }
        let store = NoteStore::open(dir.path()).unwrap();
        // Every top-level entry is a directory. Enumerating just three must
        // exhaust the global budget before any child directory can be entered.
        let listing = store.list_with_entry_limit(3).unwrap();
        assert!(listing.notes.is_empty());
        assert_eq!(listing.warnings.len(), 1);
        assert!(listing.warnings[0].contains("Entry limit of 3"));
        let full = store.list_with_entry_limit(9).unwrap();
        assert_eq!(full.notes.len(), 4);
        assert!(full.warnings.is_empty());
    }

    #[test]
    fn ignored_files_and_excluded_directories_consume_entry_budget() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/not-enumerated.md"), "").unwrap();
        fs::write(dir.path().join("ignored.bin"), "").unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let limited = store.list_with_entry_limit(2).unwrap();
        assert!(limited.notes.is_empty());
        assert_eq!(limited.warnings.len(), 1);
        assert!(limited.warnings[0].contains("Entry limit of 2"));
        assert!(store.list_with_entry_limit(3).unwrap().warnings.is_empty());
    }

    #[test]
    fn create_rejects_excluded_parent_directories_but_allows_dotted_names() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        for (index, name) in [".git", ".hg", ".svn", "target", "node_modules", "TaRgEt"]
            .iter()
            .enumerate()
        {
            let parent = Path::new("ordinary").join(name).join("child");
            fs::create_dir_all(dir.path().join(&parent)).unwrap();
            let path = parent.join(format!("note-{index}.md"));
            let error = store.create(&path).unwrap_err().to_string();
            assert!(error.contains("excluded from listing"), "{error}");
            assert!(!dir.path().join(&path).exists());
            // Explicit loading remains independent of listing exclusions.
            fs::write(dir.path().join(&path), "existing").unwrap();
            assert_eq!(store.load(&path).unwrap().text, "existing");
        }
        fs::create_dir(dir.path().join(".ordinary")).unwrap();
        let dotted = Path::new(".ordinary/.dotted.md");
        store.create(dotted).unwrap();
        assert!(store
            .list()
            .unwrap()
            .notes
            .iter()
            .any(|note| note.path == dotted));
    }

    #[test]
    fn creation_depth_matches_listing_depth_exactly() {
        let dir = tempdir().unwrap();
        let store = NoteStore::open(dir.path()).unwrap();
        let mut parent = PathBuf::new();
        for _ in 0..MAX_DEPTH {
            parent.push("d");
        }
        fs::create_dir_all(dir.path().join(&parent)).unwrap();
        let allowed = parent.join("allowed.md");
        store.create(&allowed).unwrap();
        assert!(store
            .list()
            .unwrap()
            .notes
            .iter()
            .any(|note| note.path == allowed));
        parent.push("too-deep");
        fs::create_dir(dir.path().join(&parent)).unwrap();
        let rejected = parent.join("rejected.md");
        let error = store.create(&rejected).unwrap_err().to_string();
        assert!(error.contains("depth") && error.contains("16"), "{error}");
        assert!(!dir.path().join(&rejected).exists());
        fs::write(dir.path().join(&rejected), "existing").unwrap();
        assert_eq!(store.load(&rejected).unwrap().text, "existing");
    }

    #[test]
    fn child_symlinks_cannot_load_or_create_outside_root() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("outside.md"), "outside").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(outside.path(), dir.path().join("linked"));
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(outside.path(), dir.path().join("linked"));
        #[cfg(not(any(windows, unix)))]
        return;
        #[cfg(any(windows, unix))]
        if let Err(error) = linked {
            #[cfg(windows)]
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return; // Windows installations may require symlink privileges.
            }
            panic!("cannot create test symlink: {error}");
        }
        let store = NoteStore::open(dir.path()).unwrap();
        assert!(store.load(Path::new("linked/outside.md")).is_err());
        assert!(store.create(Path::new("linked/new.md")).is_err());
        assert!(store.load(&outside.path().join("outside.md")).is_err());
        assert!(store.list().unwrap().notes.is_empty());
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(
            outside.path().join("outside.md"),
            dir.path().join("file.md"),
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("outside.md"),
            dir.path().join("file.md"),
        )
        .unwrap();
        assert!(store.load(Path::new("file.md")).is_err());
        assert!(store.create(Path::new("file.md")).is_err());
        assert!(store.list().unwrap().notes.is_empty());
        assert!(!outside.path().join("new.md").exists());
    }
}
