use crate::theme::{self, BG, FG};
use crate::{
    commands::{Command, Palette},
    explorer::{self, Node},
    i18n::Language,
    links::{extract_links, resolve_link, NoteLink},
    markdown::{plain_text, render_markdown},
    storage::{Document, NoteEntry, NoteStore, MAX_NOTE_BYTES},
    updates::{self, UpdateStatus},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant},
};
use tui_textarea::TextArea;

const PREVIEW_BYTES: usize = 256 * 1024;
const PREVIEW_LINES: usize = 4096;
const EDITOR_LINES: usize = 50_000;

// Filtering this shared order preserves both Browse and Edit palette ordering.
const PALETTE_COMMAND_ORDER: [Command; 15] = [
    Command::NewNote,
    Command::SearchNotes,
    Command::Refresh,
    Command::EditNote,
    Command::OpenLinks,
    Command::SaveNote,
    Command::Back,
    Command::ToggleFocus,
    Command::Undo,
    Command::Redo,
    Command::Language,
    Command::Help,
    Command::Quit,
    Command::CheckUpdates,
    Command::UpdateResult,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Browse,
    Edit,
    Search,
    New,
    Links,
    Confirm { quit: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum OverlayKind {
    Help,
    Language,
    UpdateConsent,
    UpdateResult,
}

// Dialog lifetime and rendered hit targets never belong to the underlying editor.
// Closing or interrupting a dialog drops all of its temporary interaction state.
struct Overlay {
    kind: OverlayKind,
    return_mode: Mode,
    visible: bool,
    selected: usize,
    hits: Vec<(Rect, usize)>,
}

impl Overlay {
    fn new(kind: OverlayKind, return_mode: Mode) -> Self {
        Self {
            kind,
            return_mode,
            visible: false,
            selected: 0,
            hits: Vec::new(),
        }
    }
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
    palette: Option<Palette>,
    palette_visible: bool,
    palette_hits: Vec<(Rect, Command)>,
    notice_until: Option<Instant>,
    expanded: HashSet<PathBuf>,
    explorer_focus: Option<Node>,
    explorer_hits: Vec<(Rect, Node)>,
    overlay: Option<Overlay>,
    language: Language,
    config_file: Option<PathBuf>,
    update_rx: Option<Receiver<updates::CheckResult>>,
    update_result: Option<UpdateStatus>,
    #[cfg(test)]
    dirty_comparisons: std::cell::Cell<usize>,
    update_worker: fn() -> Result<Receiver<updates::CheckResult>>,
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
            palette: None,
            palette_visible: false,
            palette_hits: Vec::new(),
            notice_until: Some(Instant::now() + Duration::from_secs(4)),
            expanded: HashSet::from([PathBuf::new()]),
            explorer_focus: None,
            explorer_hits: Vec::new(),
            overlay: None,
            language: Language::English,
            config_file: None,
            update_rx: None,
            update_result: None,
            #[cfg(test)]
            dirty_comparisons: std::cell::Cell::new(0),
            update_worker: updates::start_check,
        };
        // Unit tests cannot accidentally perform a live check, even after visible consent.
        #[cfg(test)]
        {
            app.update_worker =
                || panic!("Tests must inject an update worker; live HTTP is forbidden");
        }
        app.refresh(None)?;
        Ok(app)
    }

    fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.error = error;
        self.notice_until = Some(Instant::now() + Duration::from_secs(if error { 8 } else { 4 }));
    }
    fn tr(&self, english: &'static str, russian: &'static str) -> &'static str {
        self.language.text(english, russian)
    }
    fn select_language(&mut self) {
        let Some(overlay) = self
            .overlay
            .as_ref()
            .filter(|overlay| overlay.kind == OverlayKind::Language && overlay.visible)
        else {
            return;
        };
        let language = if overlay.selected == 0 {
            Language::English
        } else {
            Language::Russian
        };
        if let Some(path) = &self.config_file {
            if let Err(error) = crate::config::set_language_at(path, language) {
                self.message(
                    format!(
                        "{}: {error}",
                        self.tr("Cannot save language", "Не удалось сохранить язык")
                    ),
                    true,
                );
                return;
            }
        }
        self.language = language;
        self.close_overlay();
        self.message(self.tr("Language changed", "Язык изменён"), false);
        if self.editor.is_none() {
            let scroll = self.scroll;
            self.load_selected();
            self.scroll = scroll;
        }
    }
    fn selected_path(&self) -> Option<PathBuf> {
        if self.query.is_empty() {
            if let Some(node) = &self.explorer_focus {
                return match node {
                    Node::Note(path) => Some(path.clone()),
                    Node::Folder(_) => None,
                };
            }
        }
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
        let preferred_tree = preferred.clone();
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
        if self.query.is_empty() {
            if let Some(path) = preferred_tree {
                self.expand_parents(&path);
                self.explorer_focus = Some(Node::Note(path));
            }
            let rows = explorer::rows(&self.notes, &self.expanded);
            if !rows
                .iter()
                .any(|row| Some(&row.node) == self.explorer_focus.as_ref())
            {
                self.explorer_focus = self
                    .visible
                    .first()
                    .map(|index| Node::Note(self.notes[*index].path.clone()))
                    .filter(|node| rows.iter().any(|row| row.node == *node))
                    .or_else(|| rows.first().map(|row| row.node.clone()));
            }
        }
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
                    let bounded = preview_source(&doc.text, self.language);
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
                Err(e) => self.message(
                    format!(
                        "{}: {}",
                        self.tr("Cannot read note", "Не удалось прочитать заметку"),
                        self.language.error(&e.to_string())
                    ),
                    true,
                ),
            }
        }
        if self.document.is_none() {
            let text = if self.query.is_empty()
                && matches!(self.explorer_focus, Some(Node::Folder(_)))
            {
                self.tr("Folder selected.\n\nEnter / → expand · ← collapse or parent\nn create a note in this folder\nCtrl+P opens commands\n\nNotes remain ordinary local UTF-8 files.", "Выбрана папка.\n\nEnter / → раскрыть · ← свернуть / родитель\nn создать заметку в этой папке\nCtrl+P открывает команды\n\nЗаметки — обычные локальные UTF-8 файлы.")
            } else if self.visible.is_empty() {
                if self.query.is_empty() {
                    self.tr("No notes yet.\n\nPress n to create your first .md note.\nNotes are ordinary UTF-8 .md / .txt files.\n\nUse [[Note]] to link another note.", "Пока нет заметок.\n\nn — создать первую .md заметку.\nЗаметки — обычные UTF-8 .md / .txt файлы.\n\n[[Заметка]] — ссылка на другую заметку.")
                } else {
                    self.tr(
                        "No matching notes.\nEsc clears the filter.",
                        "Совпадений нет.\nEsc сбрасывает поиск.",
                    )
                }
            } else {
                self.tr(
                    "Note unavailable. See the notification; r retries loading.",
                    "Заметка недоступна. Подробности в уведомлении; r — повторить.",
                )
            };
            self.preview = Paragraph::new(plain_text(text)).wrap(Wrap { trim: false });
        }
    }

    fn select(&mut self, delta: isize) {
        if self.query.is_empty() {
            let rows = explorer::rows(&self.notes, &self.expanded);
            let index = rows
                .iter()
                .position(|row| Some(&row.node) == self.explorer_focus.as_ref())
                .unwrap_or(0);
            if let Some(row) = rows.get(
                index
                    .saturating_add_signed(delta)
                    .min(rows.len().saturating_sub(1)),
            ) {
                self.explorer_focus = Some(row.node.clone());
                self.load_selected();
            }
            return;
        }
        if self.visible.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.visible.len() - 1);
        self.load_selected();
    }

    fn expand_parents(&mut self, path: &Path) {
        let mut current = path.parent();
        while let Some(parent) = current {
            self.expanded.insert(parent.to_owned());
            current = parent.parent();
        }
    }
    fn open_explorer(&mut self, toggle: bool) {
        if let Some(Node::Folder(path)) = self.explorer_focus.clone() {
            if !self.expanded.insert(path.clone()) {
                if toggle {
                    self.expanded.remove(&path);
                } else {
                    let rows = explorer::rows(&self.notes, &self.expanded);
                    let folder = Node::Folder(path);
                    if let Some(index) = rows.iter().position(|row| row.node == folder) {
                        if let Some(child) = rows
                            .get(index + 1)
                            .filter(|row| row.depth > rows[index].depth)
                        {
                            self.explorer_focus = Some(child.node.clone());
                            self.load_selected();
                        }
                    }
                }
            }
        } else if toggle {
            self.execute_command(Command::EditNote);
        }
    }
    fn parent_explorer(&mut self) {
        let Some(node) = self.explorer_focus.clone() else {
            return;
        };
        if let Node::Folder(path) = &node {
            if self.expanded.remove(path) {
                return;
            }
        }
        if let Some(parent) = node.path().parent() {
            self.explorer_focus = Some(Node::Folder(parent.to_owned()));
            self.load_selected();
        }
    }

    fn start_edit(&mut self) {
        let Some(doc) = &self.document else {
            return;
        };
        if doc.text.bytes().filter(|b| *b == b'\n').count() >= EDITOR_LINES {
            self.message(self.tr("This note exceeds 50,000 editor lines. Use an external editor; the bounded preview is still available.", "Заметка длиннее 50 000 строк. Используйте внешний редактор; ограниченный предпросмотр доступен."), true);
            return;
        }
        if !editable(&doc.text) {
            self.message(self.tr("This note contains terminal/control characters. Preview is sanitized; edit it in an external editor.", "В заметке есть управляющие символы. Предпросмотр очищен; используйте внешний редактор."), true);
            return;
        }
        let mut editor = TextArea::from(doc.text.split('\n'));
        editor.set_tab_length(4);
        editor.set_style(theme::surface());
        editor.set_cursor_line_style(theme::surface().bg(theme::TOAST));
        editor.set_line_number_style(theme::surface().fg(theme::MUTED));
        editor.set_cursor_style(theme::selected());
        editor.set_selection_style(theme::selected());
        self.editor = Some(editor);
        self.mode = Mode::Edit;
        self.message(
            self.tr(
                "Editing — Ctrl+S: save · Esc: return · Ctrl+Z / Ctrl+Y: undo / redo",
                "Редактор — Ctrl+S: сохранить · Esc: назад · Ctrl+Z / Ctrl+Y: отменить / повторить",
            ),
            false,
        );
    }

    fn edited_text(&self) -> Option<String> {
        self.editor.as_ref().map(|editor| editor.lines().join("\n"))
    }
    fn dirty(&self) -> bool {
        #[cfg(test)]
        self.dirty_comparisons.set(self.dirty_comparisons.get() + 1);
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
                self.message(self.tr("Note saved", "Заметка сохранена"), false);
                true
            }
            Err(e) => {
                self.message(
                    format!(
                        "{}: {}. {}",
                        self.tr("Save failed", "Не удалось сохранить"),
                        self.language.error(&e.to_string()),
                        self.tr(
                            "Your edits are still in memory.",
                            "Ваши правки остаются в редакторе."
                        )
                    )
                    .replace('\n', " "),
                    true,
                );
                false
            }
        }
    }

    fn leave_editor(&mut self, quit: bool) {
        self.palette = None;
        self.overlay = None;
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
                self.message(
                    format!(
                        "{}: {e}",
                        self.tr("Refresh failed", "Не удалось перечитать заметки")
                    ),
                    true,
                );
            }
        }
    }

    fn create(&mut self) {
        let name = self.input.trim();
        if name.is_empty() {
            self.message(
                self.tr("Enter a note name", "Введите название заметки"),
                true,
            );
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
                    self.message(
                        format!(
                            "{}: {e}",
                            self.tr("Refresh failed", "Не удалось перечитать заметки")
                        ),
                        true,
                    );
                }
                self.document = Some(doc);
                self.start_edit();
            }
            Err(e) => self.message(
                format!(
                    "{}: {}",
                    self.tr("Cannot create note", "Не удалось создать заметку"),
                    self.language.error(&e.to_string())
                ),
                true,
            ),
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
                self.tr(
                    "No internal links. Use [[Note]] or [label](Note.md).",
                    "Внутренних ссылок нет. Используйте [[Заметка]] или [текст](Заметка.md).",
                ),
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
            Err(e) => self.message(
                format!(
                    "{}: {}",
                    self.tr("Cannot open link", "Не удалось открыть ссылку"),
                    self.language.error(&e.to_string())
                ),
                true,
            ),
        }
    }

    fn navigate(&mut self, path: PathBuf) {
        self.query.clear();
        self.filter(Some(path));
    }

    // Contextual availability is shared by shortcuts and palette enumeration.
    // Global Quit intentionally bypasses this policy in execute_command.
    fn command_available(&self, command: Command) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        match command {
            Command::NewNote | Command::SearchNotes | Command::Refresh | Command::ToggleFocus => {
                self.mode == Mode::Browse
            }
            Command::EditNote | Command::OpenLinks => {
                self.mode == Mode::Browse && self.document.is_some()
            }
            Command::SaveNote | Command::Undo | Command::Redo => self.mode == Mode::Edit,
            Command::Back => {
                self.mode == Mode::Edit || (self.mode == Mode::Browse && !self.history.is_empty())
            }
            Command::Language | Command::Help | Command::Quit | Command::CheckUpdates => {
                matches!(self.mode, Mode::Browse | Mode::Edit)
            }
            Command::UpdateResult => {
                matches!(self.mode, Mode::Browse | Mode::Edit) && self.update_result.is_some()
            }
        }
    }
    fn available_commands(&self) -> Vec<Command> {
        PALETTE_COMMAND_ORDER
            .into_iter()
            .filter(|command| self.command_available(*command))
            .collect()
    }
    fn open_palette(&mut self) {
        if !PALETTE_COMMAND_ORDER
            .into_iter()
            .any(|command| self.command_available(command))
        {
            return;
        }
        self.palette = Some(Palette::default());
        self.palette_visible = false;
    }
    fn execute_command(&mut self, command: Command) {
        // Quit is global, including dialogs, input modes and the palette.
        if command == Command::Quit {
            self.palette = None;
            self.overlay = None;
            if self.editor.is_some() {
                self.leave_editor(true);
            } else {
                self.exit = true;
            }
            return;
        }
        if !self.command_available(command) {
            return;
        }
        self.palette = None;
        match command {
            Command::NewNote => {
                let parent = match &self.explorer_focus {
                    Some(Node::Folder(path)) if self.query.is_empty() => Some(path.as_path()),
                    Some(Node::Note(path)) if self.query.is_empty() => path.parent(),
                    _ => None,
                };
                self.input = parent
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| format!("{}/", path.display()))
                    .unwrap_or_default();
                self.mode = Mode::New;
            }
            Command::EditNote => self.start_edit(),
            Command::SaveNote => {
                self.save();
            }
            Command::SearchNotes => {
                self.input.clone_from(&self.query);
                self.mode = Mode::Search;
            }
            Command::OpenLinks => self.open_links(),
            Command::Back => {
                if self.editor.is_some() {
                    self.leave_editor(false);
                } else if let Some(path) = self.history.pop() {
                    self.navigate(path);
                }
            }
            Command::Refresh => {
                self.message(self.tr("Notes refreshed", "Заметки перечитаны"), false);
                if let Err(error) = self.refresh(None) {
                    self.message(
                        format!(
                            "{}: {error}",
                            self.tr("Refresh failed", "Не удалось перечитать заметки")
                        ),
                        true,
                    );
                }
            }
            Command::ToggleFocus => self.preview_focus = !self.preview_focus,
            Command::CheckUpdates => {
                if self.update_rx.is_some() {
                    self.message(
                        self.tr(
                            "Update check already running",
                            "Проверка обновлений уже идёт",
                        ),
                        false,
                    );
                } else {
                    self.open_overlay(OverlayKind::UpdateConsent);
                }
            }
            Command::UpdateResult => {
                self.open_overlay(OverlayKind::UpdateResult);
            }
            Command::Help => {
                self.open_overlay(OverlayKind::Help);
            }
            Command::Language => {
                self.status.clear();
                self.notice_until = None;
                self.error = false;
                self.open_overlay(OverlayKind::Language);
                self.overlay.as_mut().unwrap().selected =
                    usize::from(self.language == Language::Russian);
            }
            Command::Quit => unreachable!("global quit handled above"),
            Command::Undo => {
                if let Some(editor) = &mut self.editor {
                    editor.undo();
                }
            }
            Command::Redo => {
                if let Some(editor) = &mut self.editor {
                    editor.redo();
                }
            }
        }
    }
    fn palette_key(&mut self, key: KeyEvent) {
        let commands = self
            .palette
            .as_ref()
            .unwrap()
            .filtered(&self.available_commands());
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Enter if self.palette_visible => {
                if let Some(command) = commands.get(self.palette.as_ref().unwrap().selected) {
                    self.execute_command(*command);
                }
            }
            _ => self.palette.as_mut().unwrap().key(key, commands.len()),
        }
    }
    fn open_overlay(&mut self, kind: OverlayKind) {
        self.overlay = Some(Overlay::new(kind, self.mode));
    }
    fn close_overlay(&mut self) {
        if let Some(overlay) = self.overlay.take() {
            self.mode = overlay.return_mode;
        }
    }
    fn start_update_check(&mut self) {
        if !self
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.kind == OverlayKind::UpdateConsent && overlay.visible)
        {
            return;
        }
        self.close_overlay();
        if self.update_rx.is_some() {
            return;
        }
        self.update_result = None;
        match (self.update_worker)() {
            Ok(receiver) => {
                self.update_rx = Some(receiver);
                self.message(
                    self.tr(
                        "Checking GitHub for updates…",
                        "Проверяем обновления на GitHub…",
                    ),
                    false,
                );
            }
            Err(error) => self.message(
                format!(
                    "{}: {}",
                    self.tr(
                        "Cannot check for updates",
                        "Не удалось проверить обновления"
                    ),
                    self.language.error(&error.to_string())
                ),
                true,
            ),
        }
    }
    fn poll_update(&mut self) -> bool {
        let Some(receiver) = &self.update_rx else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err(self
                .tr(
                    "Update check was interrupted. Please retry.",
                    "Проверка обновлений прервана. Повторите попытку.",
                )
                .into()),
        };
        self.update_rx = None;
        match result {
            Ok(status) => {
                self.message(
                    match &status {
                        UpdateStatus::Available(version) => format!(
                            "{} {version}. {}",
                            self.tr("Available:", "Доступна версия:"),
                            self.tr(
                                "Open the update result in Ctrl+P.",
                                "Результат проверки — в Ctrl+P."
                            )
                        ),
                        _ => status.description(self.language),
                    },
                    false,
                );
                self.update_result = Some(status);
            }
            Err(error) => self.message(
                format!(
                    "{}: {}",
                    self.tr("Update check failed", "Ошибка проверки обновлений"),
                    self.language.error(&error)
                ),
                true,
            ),
        }
        true
    }
    fn tick(&mut self, now: Instant) -> bool {
        let changed = self.poll_update();
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice_until = None;
            self.status.clear();
            self.error = false;
            return true;
        }
        changed
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
            self.execute_command(Command::Quit);
            return;
        }
        if ctrl && ctrl_char(key) == Some('p') {
            if self.palette.is_some() {
                self.palette = None;
            } else {
                self.open_palette();
            }
            return;
        }
        if self.palette.is_some() {
            self.palette_key(key);
            return;
        }
        if let Some(overlay) = &self.overlay {
            match overlay.kind {
                OverlayKind::UpdateConsent => match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Enter => self.start_update_check(),
                    _ => {}
                },
                OverlayKind::UpdateResult => {
                    if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                        self.close_overlay();
                    }
                }
                OverlayKind::Language => match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Up | KeyCode::BackTab => self.overlay.as_mut().unwrap().selected = 0,
                    KeyCode::Down | KeyCode::Tab => self.overlay.as_mut().unwrap().selected = 1,
                    KeyCode::Enter => self.select_language(),
                    _ => {}
                },
                OverlayKind::Help => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) {
                        self.close_overlay();
                    }
                }
            }
            return;
        }
        match self.mode {
            Mode::Edit => {
                if key.code == KeyCode::Esc {
                    self.execute_command(Command::Back);
                } else if ctrl && ctrl_char(key) == Some('s') {
                    self.execute_command(Command::SaveNote);
                } else if ctrl && ctrl_char(key) == Some('z') {
                    self.execute_command(Command::Undo);
                } else if ctrl && ctrl_char(key) == Some('y') {
                    self.execute_command(Command::Redo);
                } else if let Some(editor) = &mut self.editor {
                    editor.input(key);
                }
            }
            Mode::Confirm { quit } => match key.code {
                KeyCode::Char('s' | 'S') => {
                    if self.save() {
                        self.finish_edit(quit);
                    }
                }
                KeyCode::Char('d' | 'D') => {
                    self.finish_edit(quit);
                }
                KeyCode::Esc | KeyCode::Char('c' | 'C') => self.mode = Mode::Edit,
                _ => {}
            },
            Mode::Search | Mode::New => match key.code {
                KeyCode::Esc => {
                    if self.mode == Mode::Search {
                        self.query.clear();
                        self.filter(None);
                    }
                    self.mode = Mode::Browse;
                }
                KeyCode::Enter => {
                    if self.mode == Mode::New {
                        self.create();
                    } else {
                        self.mode = Mode::Browse;
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    self.update_search();
                }
                KeyCode::Char(c)
                    if !ctrl && !c.is_control() && self.input.len() + c.len_utf8() <= 1024 =>
                {
                    self.input.push(c);
                    self.update_search();
                }
                _ => {}
            },
            Mode::Links => match key.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Up => self.link_index = self.link_index.saturating_sub(1),
                KeyCode::Down => {
                    self.link_index = (self.link_index + 1).min(self.links.len().saturating_sub(1))
                }
                KeyCode::Enter => self.follow_link(),
                _ => {}
            },
            Mode::Browse => match key.code {
                KeyCode::Char('q') => self.execute_command(Command::Quit),
                KeyCode::Up if self.preview_focus => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down if self.preview_focus => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Up => self.select(-1),
                KeyCode::Down => self.select(1),
                KeyCode::Home if self.query.is_empty() => {
                    self.explorer_focus = explorer::rows(&self.notes, &self.expanded)
                        .first()
                        .map(|row| row.node.clone());
                    self.load_selected();
                }
                KeyCode::End if self.query.is_empty() => {
                    self.explorer_focus = explorer::rows(&self.notes, &self.expanded)
                        .last()
                        .map(|row| row.node.clone());
                    self.load_selected();
                }
                KeyCode::Home => {
                    self.selected = 0;
                    self.load_selected();
                }
                KeyCode::End => {
                    self.selected = self.visible.len().saturating_sub(1);
                    self.load_selected();
                }
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(12),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(12),
                KeyCode::Tab | KeyCode::BackTab => self.execute_command(Command::ToggleFocus),
                KeyCode::Enter if self.query.is_empty() && !self.preview_focus => {
                    self.open_explorer(true)
                }
                KeyCode::Right if self.query.is_empty() && !self.preview_focus => {
                    self.open_explorer(false)
                }
                KeyCode::Left
                    if self.query.is_empty() && !self.preview_focus && key.modifiers.is_empty() =>
                {
                    self.parent_explorer()
                }
                KeyCode::Enter => self.execute_command(Command::EditNote),
                KeyCode::Char('n') => self.execute_command(Command::NewNote),
                KeyCode::Char('/') => self.execute_command(Command::SearchNotes),
                KeyCode::Esc => {
                    self.query.clear();
                    self.filter(None);
                }
                KeyCode::Char('l') => self.execute_command(Command::OpenLinks),
                KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.execute_command(Command::Back);
                }
                KeyCode::Char('r') => self.execute_command(Command::Refresh),
                KeyCode::Char('?') => self.execute_command(Command::Help),
                _ => {}
            },
        }
    }

    fn update_search(&mut self) {
        if self.mode == Mode::Search {
            self.query.clone_from(&self.input);
            self.filter(None);
        }
    }

    fn paste(&mut self, text: String) {
        if self.overlay.is_some() {
            return;
        }
        if let Some(palette) = &mut self.palette {
            palette.paste(&text);
            return;
        }
        if matches!(self.mode, Mode::Search | Mode::New) {
            if !editable(&text)
                || text.contains(['\n', '\r', '\t'])
                || self.input.len().saturating_add(text.len()) > 1024
            {
                self.message(
                    self.tr(
                        "Paste rejected: enter a single line of at most 1024 bytes",
                        "Вставка отклонена: одна строка длиной не более 1024 байт",
                    ),
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
            self.message(
                self.tr(
                    "Paste rejected: unsupported control characters",
                    "Вставка отклонена: недопустимые управляющие символы",
                ),
                true,
            );
            return;
        }
        let current_size = self.edited_text().map(|s| s.len()).unwrap_or(0);
        if current_size.saturating_add(text.len()) > MAX_NOTE_BYTES {
            self.message(
                self.tr(
                    "Paste rejected: note size limit is 2 MiB",
                    "Вставка отклонена: лимит заметки — 2 МиБ",
                ),
                true,
            );
            return;
        }
        if let Some(editor) = &mut self.editor {
            editor.insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let point = (event.column, event.row).into();
        if let Some(overlay) = &mut self.overlay {
            if overlay.kind == OverlayKind::Language
                && overlay.visible
                && event.kind == MouseEventKind::Down(MouseButton::Left)
            {
                if let Some((_, index)) = overlay.hits.iter().find(|(rect, _)| rect.contains(point))
                {
                    overlay.selected = *index;
                    self.select_language();
                }
            }
            return;
        }
        if self.palette.is_some() {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                let command = self
                    .palette_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                    .map(|(_, command)| *command);
                if let Some(command) = command {
                    self.execute_command(command);
                }
            }
            return;
        }
        if self.mode != Mode::Browse {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.query.is_empty() => {
                let node = self
                    .explorer_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                    .map(|(_, node)| node.clone());
                if let Some(node) = node {
                    let folder = matches!(node, Node::Folder(_));
                    self.preview_focus = false;
                    self.explorer_focus = Some(node);
                    self.load_selected();
                    if folder {
                        self.open_explorer(true);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .explorer_hits
                    .iter()
                    .any(|(rect, _)| rect.contains(point))
                {
                    self.select(3);
                } else {
                    self.scroll = self.scroll.saturating_add(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if self
                    .explorer_hits
                    .iter()
                    .any(|(rect, _)| rect.contains(point))
                {
                    self.select(-3);
                } else {
                    self.scroll = self.scroll.saturating_sub(3);
                }
            }
            _ => {}
        }
    }
    fn draw_explorer(&mut self, frame: &mut Frame, area: Rect) {
        let rows = explorer::rows(&self.notes, &self.expanded);
        let selected = rows
            .iter()
            .position(|row| Some(&row.node) == self.explorer_focus.as_ref());
        let block = panel(
            &format!(
                " {} · {} ",
                self.tr("Explorer", "Проводник"),
                self.notes.len()
            ),
            !self.preview_focus,
        )
        .padding(Padding::new(0, 0, 1, 0));
        let inner = block.inner(area);
        let items: Vec<_> = rows
            .iter()
            .map(|row| {
                let icon = match &row.node {
                    Node::Folder(path) => {
                        if self.expanded.contains(path) {
                            "▾ ▰ "
                        } else {
                            "▸ ▰ "
                        }
                    }
                    Node::Note(_) => "  · ",
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(row.depth)),
                    Span::styled(icon, Style::default().fg(theme::MUTED)),
                    Span::raw(row.label.as_str()),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .style(theme::surface())
                .highlight_style(Style::default().bg(theme::TOAST_EDGE).fg(FG)),
            area,
            &mut state,
        );
        for (index, row) in rows
            .iter()
            .enumerate()
            .skip(state.offset())
            .take(usize::from(inner.height))
        {
            self.explorer_hits.push((
                Rect::new(
                    inner.x,
                    inner.y + (index - state.offset()) as u16,
                    inner.width,
                    1,
                ),
                row.node.clone(),
            ));
        }
    }
    fn draw_notice(&self, frame: &mut Frame) {
        if self.notice_until.is_none() || self.status.is_empty() {
            return;
        }
        let area = frame.area();
        let width = (area.width / 2)
            .clamp(30, 52)
            .min(area.width.saturating_sub(2));
        if width < 8 || area.height < 7 {
            return;
        }
        let mut lines = wrap_plain_text(&crate::safe(&self.status), usize::from(width - 6));
        let height = (lines.len() + 2).min(8).min(usize::from(area.height - 3)) as u16;
        let rect = Rect::new(area.right() - width - 1, area.y + 2, width, height);
        let style = theme::surface().bg(theme::TOAST);
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(style), rect);
        for (x, color) in [
            (rect.x, theme::TOAST_EDGE),
            (
                rect.x + 1,
                if self.error {
                    theme::ERROR
                } else {
                    theme::TOAST_EDGE
                },
            ),
            (rect.right() - 1, theme::TOAST_EDGE),
        ] {
            frame.render_widget(
                Paragraph::new((0..height).map(|_| Line::from("▏")).collect::<Vec<_>>())
                    .style(style.fg(color)),
                Rect::new(x, rect.y, 1, height),
            );
        }
        let content = Rect::new(rect.x + 3, rect.y + 1, width - 6, height - 2);
        if lines.len() > usize::from(content.height) {
            lines.truncate(usize::from(content.height));
            if let Some(last) = lines.last_mut() {
                last.pop();
                last.push('…');
            }
        }
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()).style(style),
            content,
        );
    }
    fn draw_palette(&mut self, frame: &mut Frame) {
        let Some(palette) = &self.palette else {
            return;
        };
        let available = self.available_commands();
        let commands = palette.filtered(&available);
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(76);
        let groups = available
            .iter()
            .fold((0u16, ""), |(count, previous), command| {
                (
                    count + u16::from(command.group() != previous),
                    command.group(),
                )
            })
            .0;
        let height = (available.len() as u16 + groups * 2 + 7).min(area.height.saturating_sub(4));
        if width < 30 || height < 8 {
            return;
        }
        let rect = centered(area, width, height);
        dim_backdrop(frame);
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(theme::surface()), rect);
        let inner = Rect::new(rect.x + 2, rect.y + 1, width - 4, height - 2);
        let rows = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(self.tr("Commands", "Команды"))
                .style(theme::surface().add_modifier(Modifier::BOLD)),
            Rect::new(inner.x, inner.y, inner.width - 4, 1),
        );
        frame.render_widget(
            Paragraph::new("esc").style(theme::surface().fg(theme::MUTED)),
            Rect::new(inner.right() - 3, inner.y, 3, 1),
        );
        let query = if palette.query.is_empty() {
            Line::from(vec![
                Span::styled(self.tr("S", "П"), theme::selected()),
                Span::styled(
                    self.tr("earch commands", "оиск команд"),
                    theme::surface().fg(theme::MUTED),
                ),
            ])
        } else {
            Line::from(format!("{}▏", palette.query))
        };
        frame.render_widget(
            Paragraph::new(query).style(theme::surface()),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
        let mut lines = Vec::new();
        let mut command_rows = Vec::new();
        let mut previous = "";
        let mut selected_row = 0;
        for (index, command) in commands.iter().enumerate() {
            if command.group() != previous {
                if !previous.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::styled(
                    command.localized_group(self.language),
                    theme::surface()
                        .fg(theme::SECONDARY)
                        .add_modifier(Modifier::BOLD),
                ));
                previous = command.group();
            }
            if index == palette.selected {
                selected_row = lines.len();
            }
            command_rows.push((lines.len(), *command));
            let style = if index == palette.selected {
                theme::selected()
            } else {
                theme::surface()
            };
            let shortcut = match (self.mode, command) {
                (Mode::Edit, Command::Help) => "",
                (Mode::Edit, Command::Back) => "Esc",
                (Mode::Browse, Command::Back) => "Alt+Left",
                _ => command.shortcut(),
            };
            let label: String = command
                .localized_label(self.language)
                .chars()
                .take(usize::from(rows[1].width).saturating_sub(shortcut.len() + 1))
                .collect();
            let padding = usize::from(rows[1].width)
                .saturating_sub(Line::from(label.as_str()).width() + shortcut.len());
            lines.push(Line::from(vec![
                Span::styled(format!("{label}{}", " ".repeat(padding)), style),
                Span::styled(
                    shortcut,
                    if index == palette.selected {
                        style
                    } else {
                        style.fg(theme::MUTED)
                    },
                ),
            ]));
        }
        if commands.is_empty() {
            lines.push(Line::styled(
                self.tr("No matching commands", "Команды не найдены"),
                theme::surface().fg(theme::MUTED),
            ));
        }
        let offset = (selected_row + 1).saturating_sub(usize::from(rows[1].height));
        for (line, command) in command_rows {
            if line >= offset && line < offset + usize::from(rows[1].height) {
                self.palette_hits.push((
                    Rect::new(
                        rows[1].x,
                        rows[1].y + (line - offset) as u16,
                        rows[1].width,
                        1,
                    ),
                    command,
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(lines)
                .style(theme::surface())
                .scroll((offset as u16, 0)),
            rows[1],
        );
        frame.render_widget(
            Paragraph::new(self.tr(
                "↑↓ select · Enter run · Esc cancel",
                "↑↓ выбор · Enter выполнить · Esc отмена",
            ))
            .style(theme::surface().fg(theme::MUTED)),
            rows[2],
        );
        self.palette_visible = true;
    }
    fn draw(&mut self, frame: &mut Frame) {
        if let Some(overlay) = &mut self.overlay {
            overlay.visible = false;
            overlay.hits.clear();
        }
        self.palette_visible = false;
        self.palette_hits.clear();
        self.explorer_hits.clear();
        // A frame-local snapshot: undo, saves and conflicts require no cache invalidation.
        let dirty = self.dirty();
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        if area.width < 60 || area.height < 12 {
            let text = if matches!(self.mode, Mode::Confirm { .. }) {
                format!(
                    "{}\n{}",
                    self.tr(
                        "Unsaved note\n[s] Save  [d] Discard\n[c/Esc] Cancel",
                        "Заметка не сохранена\n[s] Сохранить  [d] Отбросить\n[c/Esc] Отмена"
                    ),
                    crate::safe(&self.status)
                )
            } else {
                format!("{}\n{}", self.tr("TerminalNote — resize to 60 × 12\nCtrl+Q exits; Esc leaves editor.", "TerminalNote — увеличьте окно до 60 × 12\nCtrl+Q — выход; Esc — назад из редактора."), crate::safe(&self.status))
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(FG).add_modifier(if self.error {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }))
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
                Constraint::Length(1),
            ])
            .split(area);
        let heading = Line::from(vec![
            Span::styled(
                " ◆ TerminalNote ",
                Style::default()
                    .fg(theme::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", crate::safe(&self.store.root().display().to_string())),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                if dirty {
                    self.tr(" · unsaved", " · не сохранено")
                } else {
                    self.tr(" · local", " · локально")
                },
                Style::default().fg(if dirty {
                    theme::WARNING
                } else {
                    theme::SUCCESS
                }),
            ),
        ]);
        frame.render_widget(Paragraph::new(heading), parts[0]);
        if self.editor.is_some() {
            let name = self
                .document
                .as_ref()
                .map(|d| crate::safe(&d.path.display().to_string()))
                .unwrap_or_default();
            if let Some(editor) = &mut self.editor {
                editor.set_block(panel(
                    &format!(
                        " {}{} ",
                        name,
                        if dirty {
                            self.language.text(" • unsaved", " • не сохранено")
                        } else {
                            ""
                        }
                    ),
                    true,
                ));
                frame.render_widget(&*editor, parts[1]);
            }
        } else {
            let panels = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(parts[1]);
            if self.query.is_empty() {
                self.draw_explorer(frame, panels[0]);
            } else {
                let items: Vec<ListItem> = self
                    .visible
                    .iter()
                    .map(|i| ListItem::new(crate::safe(&self.notes[*i].path.to_string_lossy())))
                    .collect();
                let list = List::new(items)
                    .block(
                        panel(
                            &format!(" {} · {} ", self.tr("Notes", "Заметки"), self.visible.len()),
                            !self.preview_focus,
                        )
                        .padding(Padding::new(0, 0, 1, 0)),
                    )
                    .highlight_style(theme::selected().add_modifier(Modifier::BOLD))
                    .highlight_symbol("› ");
                let mut state = ListState::default()
                    .with_selected((!self.visible.is_empty()).then_some(self.selected));
                frame.render_stateful_widget(list, panels[0], &mut state);
            }
            let mut preview = std::mem::take(&mut self.preview)
                .block(
                    panel(
                        self.tr(
                            " Preview · Enter to edit · l for links ",
                            " Просмотр · Enter редактировать · l ссылки ",
                        ),
                        self.preview_focus,
                    )
                    .padding(Padding::new(0, 0, 1, 0)),
                )
                .style(theme::surface().fg(theme::WHITE));
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
        let help = if self.overlay.as_ref().is_some_and(|overlay| {
            matches!(
                overlay.kind,
                OverlayKind::UpdateConsent | OverlayKind::UpdateResult
            )
        }) {
            self.tr(
                " Enter continue · Esc back ",
                " Enter продолжить · Esc назад ",
            )
        } else if self.editor.is_some() {
            self.tr(" Ctrl+P commands · Ctrl+S save · Esc back · Ctrl+Z undo · Ctrl+Y redo · Ctrl+Q quit ", " Ctrl+P команды · Ctrl+S сохранить · Esc назад · Ctrl+Z отменить · Ctrl+Y повторить · Ctrl+Q выход ")
        } else {
            self.tr(" Ctrl+P commands · ↑↓ select · ←→ folders · Enter open/edit · n new · / search · Tab preview ", " Ctrl+P команды · ↑↓ выбор · ←→ папки · Enter открыть · n новая · / поиск · Tab просмотр ")
        };
        frame.render_widget(Paragraph::new(help).style(theme::dim()), parts[2]);
        self.draw_notice(frame);
        match self.overlay.as_ref().map(|overlay| overlay.kind) {
            Some(OverlayKind::UpdateConsent) => {
                let text = self.tr("Send a request to api.github.com?\n\nGitHub receives your IP address and TerminalNote/version User-Agent.\nNotes, filenames, folder paths and settings are NOT sent.\n\nOnly this command starts the check. Timeout: 10 seconds.\nNo automatic downloads, execution or installation.\n\n[Enter] Check   [Esc] Cancel", "Отправить запрос к api.github.com?\n\nGitHub получит ваш IP и User-Agent TerminalNote/версия.\nЗаметки, имена файлов, пути папок и настройки НЕ передаются.\n\nПроверка запускается только этой командой. Тайм-аут: 10 секунд.\nНет автоматического скачивания, запуска или установки.\n\n[Enter] Проверить   [Esc] Отмена");
                let height = Paragraph::new(text).wrap(Wrap { trim: false }).line_count(area.width.min(82).saturating_sub(2)) + 2;
                if height > usize::from(area.height.saturating_sub(2)) {
                    popup(frame, self.tr("Resize the window to see the full consent text.\n\nEsc cancels; Enter is disabled.", "Увеличьте окно, чтобы увидеть полный текст согласия.\n\nEsc — отмена; Enter заблокирован."), self.tr(" Check blocked ", " Проверка заблокирована "), 8);
                } else {
                    popup(frame, text, self.tr(" Check for updates ", " Проверить обновления "), height as u16);
                    self.overlay.as_mut().unwrap().visible = true;
                }
            }
            Some(OverlayKind::UpdateResult) => {
                let text = self.update_result.as_ref().map(|status| status.description(self.language)).unwrap_or_else(|| self.tr("No update result yet.", "Результат проверки отсутствует.").into());
                popup(frame, &format!("{text}\n\n{}", self.tr("Enter / Esc — back", "Enter / Esc — назад")), self.tr(" Updates ", " Обновления "), 18);
            }
            Some(OverlayKind::Language) => {
                dim_backdrop(frame);
                let rect = centered(area, 54, 8);
                frame.render_widget(Clear, rect);
                let block = panel(self.tr(" Interface language ", " Язык интерфейса "), true);
                let inner = block.inner(rect);
                frame.render_widget(block, rect);
                let rows = Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).split(inner);
                frame.render_stateful_widget(List::new([ListItem::new("English"), ListItem::new("Русский")]).style(theme::surface()).highlight_style(theme::selected()).highlight_symbol("› "), rows[0], &mut ListState::default().with_selected(Some(self.overlay.as_ref().unwrap().selected)));
                for index in 0..2 { self.overlay.as_mut().unwrap().hits.push((Rect::new(rows[0].x, rows[0].y + index as u16, rows[0].width, 1), index)); }
                let hint = if self.error { crate::safe(&self.status) } else { self.tr("↑↓ select · Enter apply · Esc cancel", "↑↓ выбор · Enter применить · Esc отмена").into() };
                frame.render_widget(Paragraph::new(hint).style(theme::surface().fg(if self.error { theme::ERROR } else { theme::MUTED })).wrap(Wrap { trim: false }), rows[1]);
                self.overlay.as_mut().unwrap().visible = true;
            }
            Some(OverlayKind::Help) => popup(frame, self.tr("Ctrl+P  Search commands (English / Russian)\n\n↑↓ select folder / note · ←→ collapse / expand\nEnter edit / open · n new note · / filter paths\nTab preview focus · PgUp/PgDn scroll · l internal links\nAlt+← history · r refresh · Ctrl+Q quit\n\nEditor: Ctrl+S save · Esc return · Ctrl+Z/Y undo/redo\nUnsaved changes always require save/discard confirmation.\nNotes are local plain-text files, not encrypted.\n\nEnter / Esc — back", "Ctrl+P  Поиск команд (английский / русский)\n\n↑↓ выбор папки / заметки · ←→ свернуть / раскрыть\nEnter открыть · n новая заметка · / поиск по пути\nTab фокус просмотра · PgUp/PgDn прокрутка · l ссылки\nAlt+← история · r перечитать · Ctrl+Q выход\n\nРедактор: Ctrl+S сохранить · Esc назад · Ctrl+Z/Y отмена/повтор\nНесохранённые правки требуют подтверждения выхода.\nЗаметки — незашифрованные локальные текстовые файлы.\n\nEnter / Esc — назад"), self.tr(" Help ", " Справка "), 16),
            None => {}
        }
        match self.mode {
            Mode::Search | Mode::New => {
                let title = if self.mode == Mode::Search { self.tr(" Filter filenames · Enter apply · Esc clear ", " Поиск по имени · Enter применить · Esc сброс ") } else { self.tr(" New note · Enter create · Esc cancel ", " Новая заметка · Enter создать · Esc отмена ") };
                let content = format!("{}▏\n{}", crate::safe(&self.input), if self.mode == Mode::New { self.tr("Name.md or existing-folder/Name.txt (default: .md)", "Имя.md или папка/Имя.txt (по умолчанию: .md)") } else { self.tr("Case-insensitive · includes relative folder names", "Без учёта регистра · включая имена папок") });
                popup(frame, &content, title, 5);
            }
            Mode::Links => {
                let rect = centered(area, 80, (self.links.len() as u16).saturating_add(3).clamp(5, 14));
                dim_backdrop(frame);
                frame.render_widget(Clear, rect);
                let items: Vec<ListItem> = self.links.iter().map(|link| ListItem::new(crate::safe(&format!("{} → {}", link.label, link.target)))).collect();
                let list = List::new(items).block(panel(self.tr(" Internal links · ↑↓ Enter · Esc back ", " Внутренние ссылки · ↑↓ Enter · Esc назад "), true)).highlight_style(theme::selected().add_modifier(Modifier::BOLD)).highlight_symbol("› ");
                frame.render_stateful_widget(list, rect, &mut ListState::default().with_selected(Some(self.link_index)));
            }
            Mode::Confirm { .. } => popup(frame, self.tr("You have unsaved changes.\n\n[s] Save and continue   [d] Discard   [c / Esc] Cancel\nSave conflicts keep your edits; they are never force-overwritten.", "Есть несохранённые изменения.\n\n[s] Сохранить и продолжить   [d] Отбросить   [c / Esc] Отмена\nПри конфликте правки остаются в редакторе; принудительной перезаписи нет."), self.tr(" Unsaved note ", " Заметка не сохранена "), 8),
            _ => {}
        }
        self.draw_palette(frame);
    }
}

fn preview_source(text: &str, language: Language) -> String {
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
        preview.push_str(language.text("\n\n```\n\n[Preview limited to 256 KiB / 4096 source lines. Open the editor for the full note.]\n", "\n\n```\n\n[Просмотр ограничен 256 КиБ / 4096 строками. Откройте редактор для полного текста.]\n"));
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
            'з' => 'p',
            c => c,
        }),
        _ => None,
    }
}
fn panel(title: &str, active: bool) -> Block<'static> {
    if active {
        theme::active_block(title)
    } else {
        theme::block(title)
    }
}
fn dim_backdrop(frame: &mut Frame) {
    let area = frame.area();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            frame.buffer_mut()[(x, y)].set_style(
                Style::default()
                    .fg(theme::DIMMED)
                    .bg(theme::BLACK)
                    .remove_modifier(Modifier::BOLD | Modifier::DIM | Modifier::REVERSED),
            );
        }
    }
}
fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut used = 0;
        for character in line.chars() {
            let cells = Line::from(character.to_string()).width();
            if used + cells > width.max(1) && !row.is_empty() {
                result.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(character);
            used += cells;
        }
        result.push(row);
    }
    result
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
    dim_backdrop(frame);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(content)
            .block(panel(title, true))
            .style(theme::dialog())
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
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn run(store: NoteStore, config_override: Option<PathBuf>) -> Result<()> {
    let mut app = App::new(store)?;
    let config = match config_override {
        Some(path) => path,
        None => crate::config::config_path()?,
    };
    app.language = crate::config::language_at(&config)?;
    app.config_file = Some(config);
    if app.status == "Welcome — n: new note · /: filter · ?: help" {
        app.status = app
            .tr(
                "Welcome — n: new note · /: filter · ?: help",
                "Добро пожаловать — n: новая заметка · /: поиск · ?: справка",
            )
            .into();
    }
    app.load_selected();
    enable_raw_mode().context("Cannot enable terminal raw mode")?;
    let _guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        previous_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    terminal.draw(|frame| app.draw(frame))?;
    while !app.exit {
        let mut changed = app.tick(Instant::now());
        if event::poll(Duration::from_millis(200))? {
            changed |= match event::read()? {
                Event::Key(key) => {
                    app.key(key);
                    true
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::Down(MouseButton::Left)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                    ) =>
                {
                    app.mouse(mouse);
                    true
                }
                Event::Paste(text) => {
                    app.paste(text);
                    true
                }
                Event::Resize(_, _) => true,
                _ => false,
            };
        }
        if changed && !app.exit {
            terminal.draw(|frame| app.draw(frame))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "screenshots.rs"]
mod screenshots;

#[cfg(test)]
#[path = "app_regressions.rs"]
mod regressions;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    pub(super) fn app() -> (tempfile::TempDir, App) {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("First.md"), "# First\n\n[[Second]]\n").unwrap();
        std::fs::write(root.path().join("Second.txt"), "Second note").unwrap();
        let app = App::new(NoteStore::open(root.path()).unwrap()).unwrap();
        (root, app)
    }
    pub(super) fn rendered(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }
    pub(super) fn screen(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }
    fn palette_command(app: &mut App, query: &str) {
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.palette.is_some());
        app.paste(query.into());
        let title = app.tr("Commands", "Команды");
        assert!(screen(&rendered(app, 110, 32)).contains(title));
        app.key(key(KeyCode::Enter));
    }
    #[test]
    fn palette_preserves_editor_and_uses_existing_unsaved_confirmation_paths() {
        let (root, mut app) = app();
        app.start_edit();
        app.paste("draft ".into());
        let draft = app.edited_text().unwrap();
        assert!(!app.available_commands().contains(&Command::NewNote));
        app.execute_command(Command::Refresh);
        assert_eq!(app.edited_text().unwrap(), draft);
        palette_command(&mut app, "справка");
        assert_eq!(app.overlay.as_ref().unwrap().kind, OverlayKind::Help);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text().unwrap(), draft);
        palette_command(&mut app, "quit");
        assert_eq!(app.mode, Mode::Confirm { quit: true });
        assert!(!app.exit);
        app.key(key(KeyCode::Esc));
        palette_command(&mut app, "back");
        assert_eq!(app.mode, Mode::Confirm { quit: false });
        app.key(key(KeyCode::Esc));
        palette_command(&mut app, "save");
        assert!(!app.dirty());
        assert_eq!(
            std::fs::read_to_string(root.path().join("First.md")).unwrap(),
            draft
        );
    }
    #[test]
    fn update_checks_require_full_visible_consent_and_cancel_without_touching_a_draft() {
        let (_root, mut app) = app();
        assert!(app.update_rx.is_none());
        app.start_edit();
        app.paste("Unsaved draft ".into());
        let draft = app.edited_text();
        palette_command(&mut app, "check updates");
        assert_eq!(
            app.overlay.as_ref().unwrap().kind,
            OverlayKind::UpdateConsent
        );
        app.key(key(KeyCode::Enter)); // No consent has been rendered.
        assert!(app.update_rx.is_none());
        let _ = rendered(&mut app, 60, 12);
        assert!(!app.overlay.as_ref().unwrap().visible);
        app.key(key(KeyCode::Enter));
        assert!(app.update_rx.is_none());
        let text = screen(&rendered(&mut app, 110, 32));
        assert!(text.contains("api.github.com"));
        assert!(app.overlay.as_ref().unwrap().visible);
        app.key(key(KeyCode::Esc)); // Never accept consent in a test: no HTTP requests.
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text(), draft);
        assert!(app.update_rx.is_none());
    }
    #[test]
    fn update_results_arrive_without_interrupting_edits_and_are_localized() {
        let (_root, mut app) = app();
        app.start_edit();
        app.paste("Preserved edit ".into());
        let draft = app.edited_text();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.update_rx = Some(receiver);
        sender
            .send(Ok(UpdateStatus::Available("99.0.0".into())))
            .unwrap();
        assert!(app.tick(Instant::now()));
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text(), draft);
        palette_command(&mut app, "update result");
        assert_eq!(
            app.overlay.as_ref().unwrap().kind,
            OverlayKind::UpdateResult
        );
        assert!(screen(&rendered(&mut app, 110, 32)).contains("99.0.0"));
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text(), draft);
        app.language = Language::Russian;
        let (sender, receiver) = std::sync::mpsc::channel();
        app.update_rx = Some(receiver);
        sender
            .send(Err("Cannot read GitHub response".into()))
            .unwrap();
        assert!(app.tick(Instant::now()));
        assert!(app.status.contains("Не удалось прочитать ответ GitHub"));
        assert_eq!(app.edited_text(), draft);
    }
    #[test]
    fn duplicate_update_commands_never_spawn_a_second_worker() {
        let (_root, mut app) = app();
        let (_sender, receiver) = std::sync::mpsc::channel();
        app.update_rx = Some(receiver);
        app.execute_command(Command::CheckUpdates);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.overlay.is_none());
        assert!(app.status.contains("already running"));
        app.start_edit();
        app.paste("draft".into());
        app.leave_editor(false);
        app.execute_command(Command::CheckUpdates);
        assert_eq!(app.mode, Mode::Confirm { quit: false });
    }
    #[test]
    fn english_is_default_and_language_switch_persists_without_changing_the_draft_or_note_text() {
        let (root, mut app) = app();
        assert_eq!(app.language, Language::English);
        let config = root.path().join("settings/config.toml");
        crate::config::set_default_root_at(root.path(), &config).unwrap();
        let configured_root = crate::config::default_root_at(&config).unwrap();
        let original_config = std::fs::read(&config).unwrap();
        app.config_file = Some(config.clone());
        app.start_edit();
        app.paste("Пользовательский текст / user text ".into());
        let draft = app.edited_text().unwrap();
        let original = std::fs::read(root.path().join("First.md")).unwrap();
        let expansion = app.expanded.clone();
        palette_command(&mut app, "language");
        assert_eq!(app.overlay.as_ref().unwrap().kind, OverlayKind::Language);
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter)); // Invisible choices cannot change settings.
        assert_eq!(app.language, Language::English);
        assert_eq!(std::fs::read(&config).unwrap(), original_config);
        let _ = rendered(&mut app, 110, 32);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.language, Language::Russian);
        assert_eq!(
            crate::config::language_at(&config).unwrap(),
            Language::Russian
        );
        assert_eq!(
            crate::config::default_root_at(&config).unwrap(),
            configured_root
        );
        assert_eq!(app.mode, Mode::Edit);
        assert_eq!(app.edited_text().unwrap(), draft);
        assert_eq!(app.expanded, expansion);
        assert_eq!(
            std::fs::read(root.path().join("First.md")).unwrap(),
            original
        );
        palette_command(&mut app, "назад");
        assert_eq!(app.mode, Mode::Confirm { quit: false });
        assert!(screen(&rendered(&mut app, 110, 32)).contains("Есть несохранённые изменения"));
        app.key(key(KeyCode::Esc));
        palette_command(&mut app, "язык интерфейса");
        let _ = rendered(&mut app, 110, 32);
        app.key(key(KeyCode::Up));
        app.key(key(KeyCode::Enter));
        assert_eq!(app.language, Language::English);
        assert_eq!(app.edited_text().unwrap(), draft);
    }
    #[test]
    fn failed_language_write_and_cancel_preserve_the_language_and_document() {
        let (root, mut app) = app();
        let config = root.path().join("invalid.toml");
        std::fs::write(&config, "language = 'unsupported'\n").unwrap();
        app.config_file = Some(config.clone());
        palette_command(&mut app, "language");
        app.key(key(KeyCode::Down));
        let _ = rendered(&mut app, 110, 32);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.language, Language::English);
        assert_eq!(app.overlay.as_ref().unwrap().kind, OverlayKind::Language);
        assert!(app.error);
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "language = 'unsupported'\n"
        );
        app.key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.selected_path(), Some("First.md".into()));
        assert!(app.document.as_ref().unwrap().text.contains("[[Second]]"));
    }
    #[test]
    fn palette_geometry_is_centered_stable_and_dims_the_whole_backdrop() {
        let (_root, mut app) = app();
        app.open_palette();
        for (width, height) in [(110, 40), (80, 24), (60, 12)] {
            let before = rendered(&mut app, width, height);
            let bounds = |buffer: &ratatui::buffer::Buffer| {
                let cells: Vec<_> = (0..height)
                    .flat_map(|y| (0..width).map(move |x| (x, y)))
                    .filter(|&(x, y)| buffer[(x, y)].bg == theme::SURFACE)
                    .collect();
                (
                    cells.iter().map(|cell| cell.0).min().unwrap(),
                    cells.iter().map(|cell| cell.1).min().unwrap(),
                    cells.iter().map(|cell| cell.0).max().unwrap(),
                    cells.iter().map(|cell| cell.1).max().unwrap(),
                )
            };
            let rect = bounds(&before);
            assert!((i32::from(rect.0) - i32::from(width - rect.2 - 1)).abs() <= 1);
            assert!((i32::from(rect.1) - i32::from(height - rect.3 - 1)).abs() <= 1);
            assert_eq!(before[(0, 0)].fg, theme::DIMMED);
            app.paste("search".into());
            assert_eq!(bounds(&rendered(&mut app, width, height)), rect);
            app.palette.as_mut().unwrap().query.clear();
        }
    }
    #[test]
    fn palette_click_dispatches_the_visible_action_and_invisible_palette_cannot_execute() {
        let (_root, mut app) = app();
        app.open_palette();
        app.paste("new".into());
        app.key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Browse);
        let _ = rendered(&mut app, 40, 8);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Browse);
        app.palette.as_mut().unwrap().query.clear();
        let _ = rendered(&mut app, 110, 32);
        let rect = app
            .palette_hits
            .iter()
            .find(|(_, command)| *command == Command::SearchNotes)
            .unwrap()
            .0;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::Search);
        assert!(app.palette.is_none());
    }
    #[test]
    fn notifications_are_gray_edged_top_right_expire_without_input_and_do_not_duplicate_in_footer()
    {
        let (_root, mut app) = app();
        app.start_edit();
        let draft = app.edited_text();
        app.message("Note saved", false);
        let until = app.notice_until.unwrap();
        let buffer = rendered(&mut app, 110, 32);
        assert_eq!(screen(&buffer).matches("Note saved").count(), 1);
        assert_eq!(buffer[(57, 2)].fg, theme::TOAST_EDGE);
        assert_eq!(buffer[(108, 2)].fg, theme::TOAST_EDGE);
        assert_eq!(buffer[(60, 3)].bg, theme::TOAST);
        assert_eq!(buffer[(0, 31)].fg, theme::MUTED);
        assert!(!app.tick(until - Duration::from_millis(1)));
        assert!(app.tick(until));
        assert!(!screen(&rendered(&mut app, 110, 32)).contains("Note saved"));
        assert_eq!(app.edited_text(), draft);
        assert_eq!(app.mode, Mode::Edit);
        app.message("Cannot save", true);
        let buffer = rendered(&mut app, 110, 32);
        assert_eq!(buffer[(58, 2)].fg, theme::ERROR);
    }
    #[test]
    fn explorer_collapse_search_navigation_mouse_and_note_creation_use_stable_paths() {
        let (root, mut app) = app();
        std::fs::create_dir_all(root.path().join("Work/Mail")).unwrap();
        std::fs::write(root.path().join("Work/Mail/Inbox.md"), "NESTED_PREVIEW").unwrap();
        app.refresh(None).unwrap();
        assert!(!explorer::rows(&app.notes, &app.expanded)
            .iter()
            .any(|row| row.node == Node::Note("Work/Mail/Inbox.md".into())));
        let _ = rendered(&mut app, 110, 32);
        let hit = app
            .explorer_hits
            .iter()
            .find(|(_, node)| *node == Node::Folder("Work".into()))
            .unwrap()
            .0;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.x + 1,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.expanded.contains(Path::new("Work")));
        app.key(key(KeyCode::Right)); // First child, Mail.
        assert_eq!(app.explorer_focus, Some(Node::Folder("Work/Mail".into())));
        app.key(key(KeyCode::Enter)); // Expand Mail.
        app.key(key(KeyCode::Down));
        assert_eq!(app.selected_path(), Some("Work/Mail/Inbox.md".into()));
        app.key(key(KeyCode::Left));
        app.key(key(KeyCode::Left)); // Collapse Mail.
        assert!(!app.expanded.contains(Path::new("Work/Mail")));
        app.key(key(KeyCode::Char('/')));
        app.paste("Inbox".into());
        assert_eq!(app.selected_path(), Some("Work/Mail/Inbox.md".into()));
        app.key(key(KeyCode::Esc));
        app.navigate("Work/Mail/Inbox.md".into());
        assert!(app.expanded.contains(Path::new("Work/Mail")));
        app.key(key(KeyCode::Char('n')));
        assert_eq!(app.input, "Work/Mail/");
        app.paste("New".into());
        app.key(key(KeyCode::Enter));
        assert!(root.path().join("Work/Mail/New.md").is_file());
        assert_eq!(app.mode, Mode::Edit);
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
    fn every_screen_uses_the_shared_dark_palette_including_editor_and_dialogs() {
        use ratatui::backend::TestBackend;
        let (_root, mut app) = app();
        app.preview = Paragraph::new(render_markdown("# Title\n\n**bold** *italic* [[Second]] [link](Second.txt)\n\n```rust\nlet x = 1;\n```\n\n> quote\n\n---\n\n![image](none.png)"));
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        let assert_palette = |terminal: &Terminal<TestBackend>| {
            let colors = [
                BG,
                FG,
                theme::SURFACE,
                theme::TOAST,
                theme::TOAST_EDGE,
                theme::MUTED,
                theme::BORDER,
                theme::PRIMARY,
                theme::SECONDARY,
                theme::SUCCESS,
                theme::WARNING,
                theme::ERROR,
                theme::INK,
                theme::DIMMED,
            ];
            for cell in terminal.backend().buffer().content() {
                assert!(
                    colors.contains(&cell.fg),
                    "unexpected foreground: {:?}",
                    cell.fg
                );
                assert!(
                    colors.contains(&cell.bg),
                    "unexpected background: {:?}",
                    cell.bg
                );
            }
        };
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_palette(&terminal);
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.bg == theme::TOAST_EDGE));
        app.open_links();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_palette(&terminal);
        app.mode = Mode::Browse;
        app.start_edit();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_palette(&terminal);
        app.paste("unsaved".into());
        app.leave_editor(false);
        app.message("Save failed", true);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_palette(&terminal);
        app.mode = Mode::New;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_palette(&terminal);
    }

    #[test]
    fn preview_is_bounded_without_altering_document_and_can_reach_wrapped_tail() {
        use ratatui::backend::TestBackend;
        let input = "строка\n".repeat(100_000);
        let preview = preview_source(&input, Language::English);
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
