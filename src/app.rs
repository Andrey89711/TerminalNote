use crate::{
    links::{extract_links, resolve_link, NoteLink},
    markdown::{plain_text, render_markdown},
    storage::{Document, NoteEntry, NoteStore, MAX_NOTE_BYTES},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{io, path::PathBuf, time::Duration};
use tui_textarea::TextArea;

const BG: Color = Color::Rgb(17, 19, 24);
const PANEL: Color = Color::Rgb(24, 27, 34);
const FG: Color = Color::Rgb(216, 222, 233);
const DIM: Color = Color::Rgb(126, 138, 156);
const ACCENT: Color = Color::Rgb(103, 214, 202);
const PREVIEW_BYTES: usize = 256 * 1024;
const PREVIEW_LINES: usize = 4096;
const EDITOR_LINES: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Browse,
    Edit,
    Search,
    New,
    Links,
    Confirm { quit: bool },
}

struct App {
    store: NoteStore,
    notes: Vec<NoteEntry>,
    visible: Vec<usize>,
    selected: usize,
    document: Option<Document>,
    preview: Paragraph<'static>,
    preview_measure: Option<(u16, usize)>,
    editor: Option<TextArea<'static>>,
    mode: Mode,
    preview_focus: bool,
    scroll: u16,
    query: String,
    input: String,
    status: String,
    error: bool,
    links: Vec<NoteLink>,
    link_index: usize,
    history: Vec<PathBuf>,
    exit: bool,
}

impl App {
    fn new(store: NoteStore) -> Result<Self> {
        let mut app = Self {
            store,
            notes: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            document: None,
            preview: Paragraph::default(),
            preview_measure: None,
            editor: None,
            mode: Mode::Browse,
            preview_focus: false,
            scroll: 0,
            query: String::new(),
            input: String::new(),
            status: "Welcome — n: new note · /: filter · ?: help".into(),
            error: false,
            links: Vec::new(),
            link_index: 0,
            history: Vec::new(),
            exit: false,
        };
        app.refresh(None)?;
        Ok(app)
    }

    fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.error = error;
    }
    fn selected_path(&self) -> Option<PathBuf> {
        self.visible
            .get(self.selected)
            .map(|i| self.notes[*i].path.clone())
    }

    fn refresh(&mut self, preferred: Option<PathBuf>) -> Result<()> {
        let preferred = preferred.or_else(|| self.selected_path());
        let listing = self.store.list()?;
        self.notes = listing.notes;
        self.filter(preferred);
        if !listing.warnings.is_empty() {
            self.message(listing.warnings.join(" · "), true);
        }
        Ok(())
    }

    fn filter(&mut self, preferred: Option<PathBuf>) {
        let query = self.query.to_lowercase();
        self.visible = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.path.to_string_lossy().to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.selected = preferred
            .and_then(|path| {
                self.visible
                    .iter()
                    .position(|i| self.notes[*i].path == path)
            })
            .unwrap_or(0);
        self.load_selected();
    }

    fn load_selected(&mut self) {
        self.scroll = 0;
        self.document = None;
        self.preview = Paragraph::default();
        self.preview_measure = None;
        if let Some(path) = self.selected_path() {
            match self.store.load(&path) {
                Ok(doc) => {
                    let bounded = preview_source(&doc.text);
                    let content = if doc
                        .path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                    {
                        render_markdown(&bounded)
                    } else {
                        plain_text(&bounded)
                    };
                    self.preview = Paragraph::new(content).wrap(Wrap { trim: false });
                    self.document = Some(doc);
                }
                Err(e) => self.message(format!("Cannot read note: {e}"), true),
            }
        }
        if self.document.is_none() {
            let text = if self.visible.is_empty() {
                if self.query.is_empty() {
                    "No notes yet.\n\nPress n to create your first .md note.\nNotes are ordinary UTF-8 .md / .txt files.\n\nUse [[Note]] to link another note."
                } else {
                    "No matching notes.\nEsc clears the filter."
                }
            } else {
                "Note unavailable. See the message below; r retries loading."
            };
            self.preview = Paragraph::new(plain_text(text)).wrap(Wrap { trim: false });
        }
    }

    fn select(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.visible.len() - 1);
        self.load_selected();
    }

    fn start_edit(&mut self) {
        let Some(doc) = &self.document else {
            return;
        };
        if doc.text.bytes().filter(|b| *b == b'\n').count() >= EDITOR_LINES {
            self.message("This note exceeds 50,000 editor lines. Use an external editor; the bounded preview is still available.", true);
            return;
        }
        if !editable(&doc.text) {
            self.message("This note contains terminal/control characters. Preview is sanitized; edit it in an external editor.", true);
            return;
        }
        let mut editor = TextArea::from(doc.text.split('\n'));
        editor.set_tab_length(4);
        editor.set_style(Style::default().fg(FG).bg(BG));
        editor.set_cursor_line_style(Style::default().bg(PANEL));
        editor.set_line_number_style(Style::default().fg(DIM));
        self.editor = Some(editor);
        self.mode = Mode::Edit;
        self.message(
            "Editing — Ctrl+S: save · Esc: return · Ctrl+Z / Ctrl+Y: undo / redo",
            false,
        );
    }

    fn edited_text(&self) -> Option<String> {
        self.editor.as_ref().map(|editor| editor.lines().join("\n"))
    }
    fn dirty(&self) -> bool {
        self.document
            .as_ref()
            .zip(self.editor.as_ref())
            .is_some_and(|(doc, editor)| {
                !doc.text
                    .split('\n')
                    .eq(editor.lines().iter().map(String::as_str))
            })
    }

    fn save(&mut self) -> bool {
        let Some(text) = self.edited_text() else {
            return true;
        };
        let Some(doc) = &mut self.document else {
            return false;
        };
        match self.store.save(doc, &text) {
            Ok(()) => {
                self.message("Note saved", false);
                true
            }
            Err(e) => {
                self.message(
                    format!("Save failed: {e}. Your edits are still in memory.").replace('\n', " "),
                    true,
                );
                false
            }
        }
    }

    fn leave_editor(&mut self, quit: bool) {
        if self.dirty() {
            self.mode = Mode::Confirm { quit };
        } else {
            self.finish_edit(quit);
        }
    }

    fn finish_edit(&mut self, quit: bool) {
        self.editor = None;
        self.mode = Mode::Browse;
        self.exit = quit;
        if !quit {
            let preferred = self.document.as_ref().map(|doc| doc.path.clone());
            if let Err(e) = self.refresh(preferred) {
                self.message(format!("Refresh failed: {e}"), true);
            }
        }
    }

    fn create(&mut self) {
        let name = self.input.trim();
        if name.is_empty() {
            self.message("Enter a note name", true);
            return;
        }
        let mut path = PathBuf::from(name);
        if path.extension().is_none() {
            path.set_extension("md");
        }
        match self.store.create(&path) {
            Ok(doc) => {
                self.query.clear();
                if let Err(e) = self.refresh(Some(path)) {
                    self.message(format!("Refresh failed: {e}"), true);
                }
                self.document = Some(doc);
                self.start_edit();
            }
            Err(e) => self.message(format!("Cannot create note: {e}"), true),
        }
    }

    fn open_links(&mut self) {
        self.links = self
            .document
            .as_ref()
            .map(|doc| extract_links(&doc.text))
            .unwrap_or_default();
        if self.links.is_empty() {
            self.message(
                "No internal links. Use [[Note]] or [label](Note.md).",
                false,
            );
        } else {
            self.link_index = 0;
            self.mode = Mode::Links;
        }
    }

    fn follow_link(&mut self) {
        let Some(doc) = &self.document else {
            return;
        };
        let Some(link) = self.links.get(self.link_index) else {
            return;
        };
        match resolve_link(&doc.path, &link.target, &self.notes) {
            Ok(path) => {
                self.history.push(doc.path.clone());
                self.navigate(path);
                self.mode = Mode::Browse;
            }
            Err(e) => self.message(format!("Cannot open link: {e}"), true),
        }
    }

    fn navigate(&mut self, path: PathBuf) {
        self.query.clear();
        self.filter(Some(path));
    }

    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
        if ctrl && ctrl_char(key) == Some('q') {
            if self.editor.is_some() {
                self.leave_editor(true);
            } else {
                self.exit = true;
            }
            return;
        }
        match self.mode {
            Mode::Edit => {
                if key.code == KeyCode::Esc { self.leave_editor(false); }
                else if ctrl && ctrl_char(key) == Some('s') { self.save(); }
                else if ctrl && ctrl_char(key) == Some('z') { if let Some(editor) = &mut self.editor { editor.undo(); } }
                else if ctrl && ctrl_char(key) == Some('y') { if let Some(editor) = &mut self.editor { editor.redo(); } }
                else if let Some(editor) = &mut self.editor { editor.input(key); }
            }
            Mode::Confirm { quit } => match key.code {
                KeyCode::Char('s' | 'S') => { if self.save() { self.finish_edit(quit); } }
                KeyCode::Char('d' | 'D') => { self.finish_edit(quit); }
                KeyCode::Esc | KeyCode::Char('c' | 'C') => self.mode = Mode::Edit,
                _ => {}
            },
            Mode::Search | Mode::New => match key.code {
                KeyCode::Esc => { if self.mode == Mode::Search { self.query.clear(); self.filter(None); } self.mode = Mode::Browse; }
                KeyCode::Enter => { if self.mode == Mode::New { self.create(); } else { self.mode = Mode::Browse; } }
                KeyCode::Backspace => { self.input.pop(); self.update_search(); }
                KeyCode::Char(c) if !ctrl && !c.is_control() && self.input.len() + c.len_utf8() <= 1024 => {
                    self.input.push(c);
                    self.update_search();
                }
                _ => {}
            },
            Mode::Links => match key.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Up => self.link_index = self.link_index.saturating_sub(1),
                KeyCode::Down => self.link_index = (self.link_index + 1).min(self.links.len().saturating_sub(1)),
                KeyCode::Enter => self.follow_link(),
                _ => {}
            },
            Mode::Browse => match key.code {
                KeyCode::Char('q') => self.exit = true,
                KeyCode::Up if self.preview_focus => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down if self.preview_focus => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Up => self.select(-1), KeyCode::Down => self.select(1),
                KeyCode::Home => { self.selected = 0; self.load_selected(); }
                KeyCode::End => { self.selected = self.visible.len().saturating_sub(1); self.load_selected(); }
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(12),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(12),
                KeyCode::Tab | KeyCode::BackTab => self.preview_focus = !self.preview_focus,
                KeyCode::Enter => self.start_edit(),
                KeyCode::Char('n') => { self.input.clear(); self.mode = Mode::New; }
                KeyCode::Char('/') => { self.input.clone_from(&self.query); self.mode = Mode::Search; }
                KeyCode::Esc => { self.query.clear(); self.filter(None); }
                KeyCode::Char('l') => self.open_links(),
                KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => { if let Some(path) = self.history.pop() { self.navigate(path); } }
                KeyCode::Char('r') => {
                    self.message("Notes refreshed", false);
                    if let Err(e) = self.refresh(None) { self.message(format!("Refresh failed: {e}"), true); }
                }
                KeyCode::Char('?') => self.message("↑↓ select · Enter edit · Tab preview · PgUp/PgDn scroll · / filter · n new · l links · Alt+← back · r refresh · q quit", false),
                _ => {}
            }
        }
    }

    fn update_search(&mut self) {
        if self.mode == Mode::Search {
            self.query.clone_from(&self.input);
            self.filter(None);
        }
    }

    fn paste(&mut self, text: String) {
        if matches!(self.mode, Mode::Search | Mode::New) {
            if !editable(&text)
                || text.contains(['\n', '\r', '\t'])
                || self.input.len().saturating_add(text.len()) > 1024
            {
                self.message(
                    "Paste rejected: enter a single line of at most 1024 bytes",
                    true,
                );
            } else {
                self.input.push_str(&text);
                self.update_search();
            }
            return;
        }
        if self.mode != Mode::Edit {
            return;
        }
        if !editable(&text) {
            self.message("Paste rejected: unsupported control characters", true);
            return;
        }
        let current_size = self.edited_text().map(|s| s.len()).unwrap_or(0);
        if current_size.saturating_add(text.len()) > MAX_NOTE_BYTES {
            self.message("Paste rejected: note size limit is 2 MiB", true);
            return;
        }
        if let Some(editor) = &mut self.editor {
            editor.insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        if area.width < 60 || area.height < 12 {
            let text = if matches!(self.mode, Mode::Confirm { .. }) {
                format!(
                    "Unsaved note\n[s] Save  [d] Discard\n[c/Esc] Cancel\n{}",
                    crate::safe(&self.status)
                )
            } else {
                format!(
                    "TerminalNote — resize to 60 × 12\nCtrl+Q exits; Esc leaves editor.\n{}",
                    crate::safe(&self.status)
                )
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(if self.error { Color::LightRed } else { ACCENT }))
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(area);
        let heading = Line::from(vec![
            Span::styled(
                " ◆ TerminalNote ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", crate::safe(&self.store.root().display().to_string())),
                Style::default().fg(DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(heading), parts[0]);
        if self.editor.is_some() {
            let dirty = self.dirty();
            let name = self
                .document
                .as_ref()
                .map(|d| crate::safe(&d.path.display().to_string()))
                .unwrap_or_default();
            if let Some(editor) = &mut self.editor {
                editor.set_block(panel(
                    &format!(" {}{} ", name, if dirty { " • unsaved" } else { "" }),
                    true,
                ));
                frame.render_widget(&*editor, parts[1]);
            }
        } else {
            let panels = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(parts[1]);
            let items: Vec<ListItem> = self
                .visible
                .iter()
                .map(|i| ListItem::new(crate::safe(&self.notes[*i].path.to_string_lossy())))
                .collect();
            let list = List::new(items)
                .block(panel(
                    &format!(" Notes · {} ", self.visible.len()),
                    !self.preview_focus,
                ))
                .highlight_style(
                    Style::default()
                        .bg(Color::Rgb(42, 65, 71))
                        .fg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› ");
            let mut state = ListState::default()
                .with_selected((!self.visible.is_empty()).then_some(self.selected));
            frame.render_stateful_widget(list, panels[0], &mut state);
            let mut preview = std::mem::take(&mut self.preview).block(panel(
                " Preview · Enter to edit · l for links ",
                self.preview_focus,
            ));
            let rows = match self.preview_measure {
                Some((width, rows)) if width == panels[1].width => rows,
                _ => {
                    let rows = preview
                        .line_count(panels[1].width.saturating_sub(2))
                        .saturating_sub(2);
                    self.preview_measure = Some((panels[1].width, rows));
                    rows
                }
            };
            // Actual word-wrapped height is measured once per width/content.
            // Moving the cached widget avoids cloning the entire document per key.
            self.scroll = self
                .scroll
                .min(rows.saturating_sub(1).min(u16::MAX as usize) as u16);
            preview = preview.scroll((self.scroll, 0));
            frame.render_widget(&preview, panels[1]);
            self.preview = preview;
        }
        frame.render_widget(
            Paragraph::new(crate::safe(&self.status))
                .style(Style::default().fg(if self.error { Color::LightRed } else { DIM }))
                .wrap(Wrap { trim: false }),
            parts[2],
        );
        let help = if self.editor.is_some() {
            " Ctrl+S save   Esc back   Ctrl+Z undo   Ctrl+Y redo   Ctrl+Q quit "
        } else {
            " ↑↓ select   Enter edit   / filter   n new   l links   Tab preview   q quit "
        };
        frame.render_widget(
            Paragraph::new(help).style(Style::default().bg(PANEL).fg(ACCENT)),
            parts[3],
        );
        match self.mode {
            Mode::Search | Mode::New => {
                let title = if self.mode == Mode::Search { " Filter filenames · Enter apply · Esc clear " } else { " New note · Enter create · Esc cancel " };
                let content = format!("{}▏\n{}", crate::safe(&self.input), if self.mode == Mode::New { "Name.md or existing-folder/Name.txt (default: .md)" } else { "Case-insensitive · includes relative folder names" });
                popup(frame, &content, title, 5);
            }
            Mode::Links => {
                let rect = centered(area, 80, (self.links.len() as u16).saturating_add(3).clamp(5, 14));
                frame.render_widget(Clear, rect);
                let items: Vec<ListItem> = self.links.iter().map(|link| ListItem::new(crate::safe(&format!("{} → {}", link.label, link.target)))).collect();
                let list = List::new(items).block(panel(" Internal links · ↑↓ Enter · Esc back ", true)).highlight_style(Style::default().fg(ACCENT).bg(PANEL)).highlight_symbol("› ");
                frame.render_stateful_widget(list, rect, &mut ListState::default().with_selected(Some(self.link_index)));
            }
            Mode::Confirm { .. } => popup(frame, "You have unsaved changes.\n\n[s] Save and continue   [d] Discard   [c / Esc] Cancel\nSave conflicts keep your edits; they are never force-overwritten.", " Unsaved note ", 8),
            _ => {}
        }
    }
}

fn preview_source(text: &str) -> String {
    let mut end = text.len().min(PREVIEW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some((offset, _)) = text[..end].match_indices('\n').nth(PREVIEW_LINES - 1) {
        end = offset;
    }
    let mut preview = text[..end].to_owned();
    if end < text.len() {
        // Close a possible fenced block so the limit marker remains readable.
        preview.push_str("\n\n```\n\n[Preview limited to 256 KiB / 4096 source lines. Open the editor for the full note.]\n");
    }
    preview
}

fn editable(text: &str) -> bool {
    text.chars().all(|c| {
        (!c.is_control() || matches!(c, '\n' | '\r' | '\t'))
            && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
    })
}
fn ctrl_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) => Some(match c.to_lowercase().next().unwrap_or(c) {
            'ы' => 's',
            'й' => 'q',
            'я' => 'z',
            'н' => 'y',
            c => c,
        }),
        _ => None,
    }
}
fn panel(title: &str, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(Style::default().fg(if active {
            ACCENT
        } else {
            Color::Rgb(55, 63, 77)
        }))
        .style(Style::default().bg(BG).fg(FG))
}
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
fn popup(frame: &mut Frame, content: &str, title: &str, height: u16) {
    let rect = centered(frame.area(), 82, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(content)
            .block(panel(title, true))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn run(store: NoteStore) -> Result<()> {
    let mut app = App::new(store)?;
    enable_raw_mode().context("Cannot enable terminal raw mode")?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        previous_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    terminal.draw(|frame| app.draw(frame))?;
    while !app.exit {
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => app.key(key),
                Event::Paste(text) => app.paste(text),
                _ => {}
            }
            if !app.exit {
                terminal.draw(|frame| app.draw(frame))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn app() -> (tempfile::TempDir, App) {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("First.md"), "# First\n\n[[Second]]\n").unwrap();
        std::fs::write(root.path().join("Second.txt"), "Second note").unwrap();
        let app = App::new(NoteStore::open(root.path()).unwrap()).unwrap();
        (root, app)
    }
    #[test]
    fn selection_filter_and_link_navigation() {
        let (_root, mut app) = app();
        app.key(key(KeyCode::Char('l')));
        assert_eq!(app.mode, Mode::Links);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.selected_path(), Some("Second.txt".into()));
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.selected_path(), Some("First.md".into()));
        app.key(key(KeyCode::Char('/')));
        app.key(key(KeyCode::Char('s')));
        assert_eq!(app.visible.len(), 2);
        app.key(key(KeyCode::Char('e')));
        assert_eq!(app.visible.len(), 1);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.visible.len(), 2);
    }
    #[test]
    fn unsaved_cancel_discard_and_save() {
        let (root, mut app) = app();
        app.start_edit();
        app.paste("new ".into());
        assert!(app.dirty());
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Confirm { quit: false });
        app.key(key(KeyCode::Char('c')));
        assert_eq!(app.mode, Mode::Edit);
        assert!(app.save());
        assert!(!app.dirty());
        assert!(std::fs::read_to_string(root.path().join("First.md"))
            .unwrap()
            .starts_with("new "));
        app.paste("discard ".into());
        app.leave_editor(false);
        app.key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Browse);
        assert!(!app.document.as_ref().unwrap().text.contains("discard"));
    }
    #[test]
    fn external_conflict_keeps_editor_and_changes() {
        let (root, mut app) = app();
        app.start_edit();
        app.paste("mine".into());
        std::fs::write(root.path().join("First.md"), "external").unwrap();
        assert!(!app.save());
        assert!(app.dirty());
        assert!(app.editor.is_some());
        app.leave_editor(true);
        app.key(key(KeyCode::Char('s')));
        assert!(!app.exit);
        assert_eq!(
            std::fs::read_to_string(root.path().join("First.md")).unwrap(),
            "external"
        );
    }
    #[test]
    fn new_note_default_extension_and_no_clobber() {
        let (root, mut app) = app();
        app.mode = Mode::New;
        app.input = "Created".into();
        app.create();
        assert!(root.path().join("Created.md").exists());
        assert_eq!(app.mode, Mode::Edit);
        app.finish_edit(false);
        app.mode = Mode::New;
        app.input = "First.md".into();
        app.create();
        assert_eq!(app.mode, Mode::New);
        assert!(app.error);
    }
    #[test]
    fn paste_safety_and_russian_controls() {
        let (_root, mut app) = app();
        app.start_edit();
        app.paste("\x1b[31mevil".into());
        assert!(!app.dirty());
        app.paste("Привет\r\nмир".into());
        assert!(app.dirty());
        app.key(KeyEvent::new(KeyCode::Char('ы'), KeyModifiers::CONTROL));
        assert!(!app.dirty());
    }

    #[test]
    fn input_paste_does_not_trigger_commands_and_multiline_is_rejected() {
        let (_root, mut app) = app();
        app.key(key(KeyCode::Char('n')));
        app.paste("Новая заметка".into());
        assert_eq!(app.input, "Новая заметка");
        app.paste("\nq".into());
        assert_eq!(app.input, "Новая заметка");
        assert!(!app.exit);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(
            ctrl_char(KeyEvent::new(KeyCode::Char('Ы'), KeyModifiers::CONTROL)),
            Some('s')
        );
    }
    #[test]
    fn renders_browse_editor_and_small_terminal() {
        use ratatui::backend::TestBackend;
        let (_root, mut app) = app();
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("TerminalNote"));
        assert!(screen.contains("First.md"));
        assert!(screen.contains("Second.txt"));
        assert!(screen.contains("Preview"));
        assert!(screen.contains("# First"));
        app.start_edit();
        terminal.draw(|f| app.draw(f)).unwrap();
        app.paste("x".into());
        app.leave_editor(false);
        terminal.draw(|f| app.draw(f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Unsaved note"));
        let mut small = Terminal::new(TestBackend::new(40, 8)).unwrap();
        small.draw(|f| app.draw(f)).unwrap();
        let screen: String = small
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Unsaved note"));
        assert!(screen.contains("Discard"));
        assert!(screen.contains("Cancel"));
    }

    #[test]
    fn preview_is_bounded_without_altering_document_and_can_reach_wrapped_tail() {
        use ratatui::backend::TestBackend;
        let input = "строка\n".repeat(100_000);
        let preview = preview_source(&input);
        assert!(preview.contains("Preview limited"));
        assert!(preview.len() < PREVIEW_BYTES + 200);
        assert!(preview.lines().count() < PREVIEW_LINES + 10);
        let (root, mut app) = app();
        std::fs::write(
            root.path().join("First.md"),
            "abcdefghijklmnopqrst ".repeat(100),
        )
        .unwrap();
        app.load_selected();
        app.scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.scroll >= 99, "word-wrapped tail must remain reachable");
        assert_eq!(app.document.as_ref().unwrap().text.len(), 2100);
    }
}
