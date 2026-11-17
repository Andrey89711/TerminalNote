# Changelog

## Unreleased

No unreleased changes.

## 0.1.0 — 2026-11-17

- First published Windows installer ZIP and Linux release archives, with SHA-256 checksums.
- Per-user Windows setup, command PATH integration, Apps uninstall entry and safe
  removal preserving notes, configuration and unrelated files.
- Explicitly consented background GitHub update checks, available through Ctrl+P;
  no startup checks, automatic download, execution or installation.
- English-default TUI with persistent English/Russian selection through Ctrl+P →
  Language; language and notes-root settings are preserved independently.
- English documentation and refreshed renderer-based screenshot coverage.
- Shared TermVault-style dark theme with peach/purple accents across panels, editor,
  dialogs and Markdown; replaces the earlier monochrome UI.
- Centered Ctrl+P command palette with grouped actions, bilingual search, dimmed
  backdrop, shortcut labels, keyboard/mouse selection and context restrictions.
- Timed gray-edged top-right notifications instead of a persistent bottom status area.
- Explorer-style collapsible folder/note tree, keyboard/mouse navigation, full-path
  filename search and automatic ancestor expansion for note/link navigation.
- New-note prompts default to the selected folder; existing conflict-safe storage,
  unsaved-edit confirmations and UTF-8/control-character protections remain intact.
- Expanded unit and real-terminal tests; opt-in documentation screenshot generator retained.

### Initial functionality

Initial TerminalNote MVP:

- Standalone `note` command, configurable default folder and explicit path support.
- Recursive Markdown/TXT list, filename filtering, styled preview and full-screen editor.
- Save, undo/redo, new notes and unsaved-change confirmation.
- Wiki/Markdown internal links, link picker and back navigation.
- Bounded UTF-8 reads, atomic replacement and optimistic external-change detection.
- Script-friendly `--list` / `--check`, sample notes and Windows/Linux CI.
- MIT license. No cloud, database, external command execution or terminal integration.
