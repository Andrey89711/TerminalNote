//! Local note links. Resolution is lexical and never accesses the filesystem.
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::storage::NoteEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteLink {
    pub target: String,
    pub label: String,
}

fn local_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with(['/', '\\', '#'])
        && !target.contains(':')
        && !target.chars().any(char::is_control)
}

fn file_part(target: &str) -> &str {
    target.split('#').next().unwrap_or("")
}

fn note_extension(target: &str) -> bool {
    Path::new(target)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("md") || s.eq_ignore_ascii_case("txt"))
}

/// Extract wiki links and local Markdown links in source order. Percent escapes
/// are deliberately not decoded. Images, HTML, and code are not note links.
pub fn extract_links(text: &str) -> Vec<NoteLink> {
    let mut found = Vec::new();
    let mut excluded = Vec::new();
    let mut active: Option<(usize, NoteLink)> = None;
    let mut image_depth = 0usize;
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) | Event::Html(_) | Event::InlineHtml(_) => {
                excluded.push(range)
            }
            Event::Code(label) => {
                excluded.push(range);
                if image_depth == 0 {
                    if let Some((_, link)) = &mut active {
                        link.label.push_str(&label);
                    }
                }
            }
            Event::Start(Tag::Image { .. }) => {
                excluded.push(range);
                image_depth += 1;
            }
            Event::End(TagEnd::Image) => image_depth = image_depth.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => {
                excluded.push(range.clone());
                if image_depth == 0
                    && local_target(&dest_url)
                    && note_extension(file_part(&dest_url))
                {
                    active = Some((
                        range.start,
                        NoteLink {
                            target: dest_url.into_string(),
                            label: String::new(),
                        },
                    ));
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = active.take() {
                    found.push(link);
                }
            }
            Event::Text(label) if image_depth == 0 => {
                if let Some((_, link)) = &mut active {
                    link.label.push_str(&label);
                }
            }
            _ => {}
        }
    }
    excluded.sort_by_key(|range| range.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in excluded {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    let overlaps = |start: usize, end: usize| {
        let i = merged.partition_point(|range| range.end <= start);
        merged.get(i).is_some_and(|range| range.start < end)
    };
    // Scan the source, rather than parsed text, so Markdown punctuation cannot
    // manufacture a wiki link. Parser ranges exclude entire fenced/indented code.
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("[[") {
        let start = cursor + offset;
        cursor = start + 2;
        if overlaps(start, start + 2) || escaped(text, start) {
            continue;
        }
        let Some(end_offset) = text[cursor..].find("]]") else {
            break;
        };
        let end = cursor + end_offset;
        let body = &text[cursor..end];
        if body.contains(['\n', '\r', '[', ']']) || overlaps(start, end + 2) {
            continue;
        }
        cursor = end + 2;
        let (target, label) = body.split_once('|').unwrap_or((body, body));
        let target = target.trim();
        if local_target(target) {
            found.push((
                start,
                NoteLink {
                    target: target.to_owned(),
                    label: label.trim().to_owned(),
                },
            ));
        }
    }
    found.sort_by_key(|(offset, _)| *offset);
    found.into_iter().map(|(_, link)| link).collect()
}

fn escaped(text: &str, index: usize) -> bool {
    text.as_bytes()[..index]
        .iter()
        .rev()
        .take_while(|&&c| c == b'\\')
        .count()
        % 2
        == 1
}

fn normalize(base: &[String], path: &str) -> Result<Vec<String>> {
    let mut parts = base.to_vec();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("Link escapes the notes root");
                }
            }
            part => parts.push(part.to_owned()),
        }
    }
    Ok(parts)
}

fn path_matches(candidate: &[String], requested: &[String], has_extension: bool) -> bool {
    if candidate.len() != requested.len() || candidate.is_empty() {
        return false;
    }
    let last = candidate.len() - 1;
    if candidate[..last] != requested[..last] {
        return false;
    }
    let candidate = Path::new(&candidate[last]);
    if !note_extension(candidate.to_str().unwrap_or("")) {
        return false;
    }
    if has_extension {
        let requested = Path::new(&requested[last]);
        candidate.file_stem() == requested.file_stem()
            && candidate
                .extension()
                .and_then(|s| s.to_str())
                .zip(requested.extension().and_then(|s| s.to_str()))
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
    } else {
        candidate.file_stem().and_then(|s| s.to_str()) == Some(requested[last].as_str())
    }
}

fn choose(matches: Vec<&NoteEntry>, target: &str) -> Result<Option<PathBuf>> {
    match matches.as_slice() {
        [] => Ok(None),
        [note] => Ok(Some(note.path.clone())),
        _ => bail!("Ambiguous note link '{target}'; include its directory and extension"),
    }
}

/// Resolve against the supplied relative paths only. Local matches win, then
/// root-relative paths, then unique bare filenames/titles. No percent decoding.
pub fn resolve_link(current: &Path, target: &str, notes: &[NoteEntry]) -> Result<PathBuf> {
    let target = target.trim();
    if target.starts_with('#') {
        bail!("Heading-only links are not supported");
    }
    if !local_target(target) {
        bail!("Only relative local note links are supported");
    }
    let target = file_part(target);
    if target.contains('?') {
        bail!("Query strings in note links are not supported");
    }
    if target.is_empty() {
        bail!("Link has no note filename");
    }
    // Only supported note suffixes are explicit extensions. Other dots belong
    // to the stem, so [[Project.v2]] can resolve Project.v2.md.
    let has_extension = note_extension(target);
    let current = current.to_string_lossy();
    if !local_target(&current) {
        bail!("Current note must have a relative path");
    }
    let mut base = normalize(&[], &current)?;
    base.pop();
    let relative = normalize(&base, target)?;
    let indexed: Vec<_> = notes
        .iter()
        .filter_map(|note| {
            let path = note.path.to_string_lossy();
            if !local_target(&path) || !note_extension(&path) {
                return None;
            }
            normalize(&[], &path).ok().map(|parts| (note, parts))
        })
        .collect();
    if let Some(path) = choose(
        indexed
            .iter()
            .filter(|(_, p)| path_matches(p, &relative, has_extension))
            .map(|(n, _)| *n)
            .collect(),
        target,
    )? {
        return Ok(path);
    }
    let bare = !target.contains(['/', '\\']);
    if !bare {
        // A parent traversal is relative to the current note, never to root.
        if let Ok(root) = normalize(&[], target) {
            if let Some(path) = choose(
                indexed
                    .iter()
                    .filter(|(_, p)| path_matches(p, &root, has_extension))
                    .map(|(n, _)| *n)
                    .collect(),
                target,
            )? {
                return Ok(path);
            }
        }
    } else {
        let requested = vec![target.to_owned()];
        if let Some(path) = choose(
            indexed
                .iter()
                .filter(|(n, p)| {
                    p.last().is_some_and(|name| {
                        path_matches(std::slice::from_ref(name), &requested, has_extension)
                    }) || (!has_extension && n.title == target)
                })
                .map(|(n, _)| *n)
                .collect(),
            target,
        )? {
            return Ok(path);
        }
    }
    bail!("Note not found: '{target}' (URL percent escapes are not decoded)")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn notes(paths: &[&str]) -> Vec<NoteEntry> {
        paths
            .iter()
            .map(|p| NoteEntry {
                path: PathBuf::from(p),
                title: Path::new(p)
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect()
    }
    #[test]
    fn extraction_order_and_code_safety() {
        let text = "[[One]] [two](folder/two.MD#h) `[[No]]`\n```\n[[Nope]]\n```\n![image](x.md) [web](https://a/x.md) [[Folder/Three|three]]";
        let links = extract_links(text);
        assert_eq!(
            links.iter().map(|l| l.target.as_str()).collect::<Vec<_>>(),
            ["One", "folder/two.MD#h", "Folder/Three"]
        );
        assert_eq!(links[2].label, "three");
    }
    #[test]
    fn resolution_precedence_and_navigation() {
        let all = notes(&["a/One.md", "b/One.txt", "root.md", "folder/two.MD"]);
        assert_eq!(
            resolve_link(Path::new("a/current.md"), "One", &all).unwrap(),
            PathBuf::from("a/One.md")
        );
        assert_eq!(
            resolve_link(Path::new("a/current.md"), "../root.md#heading", &all).unwrap(),
            PathBuf::from("root.md")
        );
        assert_eq!(
            resolve_link(Path::new("a/current.md"), "folder/two.md", &all).unwrap(),
            PathBuf::from("folder/two.MD")
        );
        assert!(resolve_link(Path::new("current.md"), "One", &all).is_err());
    }
    #[test]
    fn reject_ambiguity_and_escapes() {
        let all = notes(&["Name.md", "Name.txt"]);
        for target in [
            "Name",
            "../Name.md",
            "/Name.md",
            "C:\\Name.md",
            "\\\\host\\Name.md",
            "https://host/Name.md",
            "#heading",
        ] {
            assert!(
                resolve_link(Path::new("current.md"), target, &all).is_err(),
                "{target}"
            );
        }
        assert!(extract_links("\\[[No]] ![x](x.md) [[https://host/a]]\n\n    [[code]]").is_empty());
    }

    #[test]
    fn dotted_root_stems_and_explicit_extensions() {
        let all = notes(&["Project.v2.md", "file.exe.md", "file.exe"]);
        for target in ["Project.v2", "Project.v2.MD", "Project.v2#heading"] {
            assert_eq!(
                resolve_link(Path::new("current.md"), target, &all).unwrap(),
                PathBuf::from("Project.v2.md")
            );
        }
        assert_eq!(
            resolve_link(Path::new("current.md"), "file.exe", &all).unwrap(),
            PathBuf::from("file.exe.md")
        );
        assert!(resolve_link(Path::new("current.md"), "project.v2", &all).is_err());
        assert!(resolve_link(Path::new("current.md"), "file", &notes(&["file.exe"])).is_err());
    }

    #[test]
    fn dotted_nested_stems_keep_resolution_precedence() {
        let all = notes(&["Project.v2.md", "a/Project.v2.txt", "folder/Other.v3.MD"]);
        for (target, expected) in [
            ("Project.v2", "a/Project.v2.txt"),
            ("../Project.v2", "Project.v2.md"),
            ("folder/Other.v3", "folder/Other.v3.MD"),
            ("Other.v3", "folder/Other.v3.MD"),
        ] {
            assert_eq!(
                resolve_link(Path::new("a/current.md"), target, &all).unwrap(),
                PathBuf::from(expected)
            );
        }
        assert!(resolve_link(Path::new("a/current.md"), "../../Project.v2", &all).is_err());
    }

    #[test]
    fn dotted_stem_ambiguity_requires_an_explicit_extension() {
        for prefix in ["", "folder/"] {
            let md = format!("{prefix}Project.v2.md");
            let txt = format!("{prefix}Project.v2.txt");
            let all = notes(&[&md, &txt]);
            let target = format!("{prefix}Project.v2");
            let error = resolve_link(Path::new("current.md"), &target, &all).unwrap_err();
            assert!(error.to_string().contains("Ambiguous"));
            assert_eq!(
                resolve_link(Path::new("current.md"), &md, &all).unwrap(),
                PathBuf::from(&md)
            );
            assert_eq!(
                resolve_link(Path::new("current.md"), &txt, &all).unwrap(),
                PathBuf::from(&txt)
            );
        }
    }
}
