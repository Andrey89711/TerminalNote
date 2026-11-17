//! A filesystem-free explorer inferred from the available note paths.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::storage::NoteEntry;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Node {
    Folder(PathBuf),
    Note(PathBuf),
}

impl Node {
    pub fn path(&self) -> &Path {
        match self {
            Self::Folder(path) | Self::Note(path) => path,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub node: Node,
    pub depth: usize,
    pub label: String,
}

/// The root is always visible; even its children require explicit expansion.
/// Only directories leading to an existing note are inferred.
pub fn rows(notes: &[NoteEntry], expanded: &HashSet<PathBuf>) -> Vec<Row> {
    let mut children: HashMap<PathBuf, HashSet<Node>> = HashMap::new();
    for note in notes {
        let mut node = Node::Note(note.path.clone());
        while let Some(parent) = node.path().parent() {
            let parent = parent.to_path_buf();
            children.entry(parent.clone()).or_default().insert(node);
            node = Node::Folder(parent);
        }
    }

    let mut result = Vec::new();
    append_rows(
        Node::Folder(PathBuf::new()),
        0,
        &children,
        expanded,
        &mut result,
    );
    result
}

fn label(node: &Node) -> String {
    if matches!(node, Node::Folder(path) if path.as_os_str().is_empty()) {
        "TerminalNote".to_owned()
    } else {
        crate::safe(
            &node
                .path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        )
    }
}

fn append_rows(
    node: Node,
    depth: usize,
    children: &HashMap<PathBuf, HashSet<Node>>,
    expanded: &HashSet<PathBuf>,
    result: &mut Vec<Row>,
) {
    let descend = matches!(&node, Node::Folder(path) if expanded.contains(path));
    let path = node.path().to_path_buf();
    result.push(Row {
        label: label(&node),
        node,
        depth,
    });
    if !descend {
        return;
    }
    if let Some(nodes) = children.get(&path) {
        let mut sorted: Vec<_> = nodes.iter().collect();
        sorted.sort_by_cached_key(|node| {
            (
                matches!(node, Node::Note(_)),
                label(node).to_lowercase(),
                node.path().to_path_buf(),
            )
        });
        for node in sorted {
            append_rows(node.clone(), depth + 1, children, expanded, result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(paths: &[&str]) -> Vec<NoteEntry> {
        paths
            .iter()
            .map(|path| NoteEntry {
                path: PathBuf::from(path),
                title: "Not the filename".to_owned(),
            })
            .collect()
    }

    fn expanded(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(|path| PathBuf::from(*path)).collect()
    }

    fn labels(rows: &[Row]) -> Vec<(&str, usize)> {
        rows.iter()
            .map(|row| (row.label.as_str(), row.depth))
            .collect()
    }

    #[test]
    fn collapsed_hierarchy_requires_each_ancestor_to_be_expanded() {
        let notes = notes(&["projects/nested/task.md", "projects/readme.txt"]);
        assert_eq!(labels(&rows(&notes, &expanded(&[]))), [("TerminalNote", 0)]);
        assert_eq!(
            labels(&rows(&notes, &expanded(&["", "projects/nested"]))),
            [("TerminalNote", 0), ("projects", 1)]
        );
        assert_eq!(
            labels(&rows(&notes, &expanded(&["", "projects"]))),
            [
                ("TerminalNote", 0),
                ("projects", 1),
                ("nested", 2),
                ("readme.txt", 2)
            ]
        );
        let visible = rows(&notes, &expanded(&["", "projects", "projects/nested"]));
        assert_eq!(
            labels(&visible),
            [
                ("TerminalNote", 0),
                ("projects", 1),
                ("nested", 2),
                ("task.md", 3),
                ("readme.txt", 2)
            ]
        );
        assert_eq!(
            visible[3].node,
            Node::Note(PathBuf::from("projects/nested/task.md"))
        );
    }

    #[test]
    fn deterministic_folders_first_and_original_path_breaks_ties() {
        let mut notes = notes(&[
            "z.md",
            "b/inside.md",
            "a.md",
            "A.md",
            "Zoo/inside.md",
            "a.md",
        ]);
        let expected = rows(&notes, &expanded(&[""]));
        assert_eq!(
            labels(&expected),
            [
                ("TerminalNote", 0),
                ("b", 1),
                ("Zoo", 1),
                ("A.md", 1),
                ("a.md", 1),
                ("z.md", 1)
            ]
        );
        notes.reverse();
        assert_eq!(rows(&notes, &expanded(&[""])), expected);
    }

    #[test]
    fn root_records_and_empty_listing() {
        let empty = rows(&[], &expanded(&[""]));
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].node, Node::Folder(PathBuf::new()));
        assert_eq!(empty[0].node.path(), Path::new(""));
        assert_eq!(labels(&empty), [("TerminalNote", 0)]);
        let visible = rows(&notes(&["welcome.md", "notes.txt"]), &expanded(&[""]));
        assert_eq!(
            labels(&visible),
            [("TerminalNote", 0), ("notes.txt", 1), ("welcome.md", 1)]
        );
        assert_eq!(visible[1].node.path(), Path::new("notes.txt"));
    }

    #[test]
    fn unicode_filenames_are_preserved_and_labels_are_sanitized() {
        let notes = notes(&["Заметки/Привет🦀.md", "é.txt", "É.txt", "bad\u{202e}.md"]);
        let visible = rows(&notes, &expanded(&["", "Заметки"]));
        assert_eq!(
            labels(&visible),
            [
                ("TerminalNote", 0),
                ("Заметки", 1),
                ("Привет🦀.md", 2),
                ("bad.md", 1),
                ("É.txt", 1),
                ("é.txt", 1)
            ]
        );
        assert_eq!(visible[3].node.path(), Path::new("bad\u{202e}.md"));
    }
}
