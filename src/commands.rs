//! Fixed action metadata and palette input; the caller supplies context-available actions.
//! Queries only filter commands: they never execute shell commands or access note data.
use crate::i18n::Language;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const MAX_QUERY_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewNote,
    EditNote,
    SaveNote,
    SearchNotes,
    OpenLinks,
    Back,
    Refresh,
    ToggleFocus,
    Help,
    Quit,
    Undo,
    Redo,
    Language,
    CheckUpdates,
    UpdateResult,
    MoveMarked,
    TrashMarked,
}

impl Command {
    pub fn label(self) -> &'static str {
        match self {
            Self::NewNote => "Create note",
            Self::EditNote => "Edit selected note",
            Self::SaveNote => "Save note",
            Self::SearchNotes => "Search notes",
            Self::OpenLinks => "Open internal links",
            Self::Back => "Go back",
            Self::Refresh => "Refresh notes",
            Self::ToggleFocus => "Switch notes / preview focus",
            Self::Help => "Show help",
            Self::Quit => "Quit",
            Self::Undo => "Undo edit",
            Self::Redo => "Redo edit",
            Self::Language => "Interface language",
            Self::CheckUpdates => "Check for updates",
            Self::UpdateResult => "Update check result",
            Self::MoveMarked => "Move marked notes",
            Self::TrashMarked => "Delete marked notes (recoverable)",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Self::NewNote | Self::EditNote | Self::SaveNote | Self::SearchNotes | Self::Refresh => {
                "Notes"
            }
            Self::OpenLinks | Self::Back | Self::ToggleFocus => "Navigation",
            Self::Undo | Self::Redo => "Editor",
            Self::Help | Self::Quit | Self::Language => "Session",
            Self::CheckUpdates | Self::UpdateResult => "Updates",
            Self::MoveMarked | Self::TrashMarked => "Notes",
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            Self::NewNote => "n",
            Self::EditNote => "Enter",
            Self::SaveNote => "Ctrl+S",
            Self::SearchNotes => "/",
            Self::OpenLinks => "l",
            Self::Back => "Alt+Left / Esc",
            Self::Refresh => "r",
            Self::ToggleFocus => "Tab / Shift+Tab",
            Self::Help => "?",
            Self::Quit => "Ctrl+Q",
            Self::Undo => "Ctrl+Z",
            Self::Redo => "Ctrl+Y",
            Self::Language | Self::CheckUpdates | Self::UpdateResult => "",
            Self::MoveMarked | Self::TrashMarked => "",
        }
    }

    pub fn aliases(self) -> &'static str {
        match self {
            Self::NewNote => "new add create note новая заметка создать добавить",
            Self::EditNote => "edit change note редактировать изменить заметку",
            Self::SaveNote => "save write note сохранить запись заметку",
            Self::SearchNotes => "search find filter notes поиск найти фильтр заметки",
            Self::OpenLinks => "open follow internal links открыть перейти внутренние ссылки",
            Self::Back => "back return cancel history назад вернуться отмена история",
            Self::Refresh => "refresh reload notes обновить перечитать заметки",
            Self::ToggleFocus => {
                "toggle switch focus preview list tab переключить фокус просмотр список"
            }
            Self::Help => "help shortcuts keys справка помощь клавиши",
            Self::Quit => "quit exit close выход выйти закрыть",
            Self::Undo => "undo revert edit отменить правку изменение",
            Self::Redo => "redo repeat edit повторить вернуть правку изменение",
            Self::Language => "language english russian язык интерфейс английский русский",
            Self::CheckUpdates => "check updates version проверить обновления версия",
            Self::UpdateResult => "update result release результат проверки обновлений релиз",
            Self::MoveMarked => "bulk move selected notes массовое перемещение переместить выбранные заметки",
            Self::TrashMarked => "bulk delete trash selected notes массовое удаление удалить выбранные заметки корзина",
        }
    }
    pub fn localized_label(self, language: Language) -> &'static str {
        language.text(
            self.label(),
            match self {
                Self::NewNote => "Создать заметку",
                Self::EditNote => "Редактировать заметку",
                Self::SaveNote => "Сохранить заметку",
                Self::SearchNotes => "Найти заметки",
                Self::OpenLinks => "Открыть внутренние ссылки",
                Self::Back => "Вернуться назад",
                Self::Refresh => "Перечитать заметки",
                Self::ToggleFocus => "Переключить фокус списка / просмотра",
                Self::Help => "Справка",
                Self::Quit => "Выйти",
                Self::Undo => "Отменить правку",
                Self::Redo => "Повторить правку",
                Self::Language => "Язык интерфейса",
                Self::CheckUpdates => "Проверить обновления",
                Self::UpdateResult => "Результат проверки обновлений",
                Self::MoveMarked => "Переместить отмеченные заметки",
                Self::TrashMarked => "Удалить отмеченные заметки (с восстановлением)",
            },
        )
    }
    pub fn localized_group(self, language: Language) -> &'static str {
        language.text(
            self.group(),
            match self.group() {
                "Notes" => "Заметки",
                "Navigation" => "Навигация",
                "Editor" => "Редактор",
                "Updates" => "Обновления",
                _ => "Приложение",
            },
        )
    }
}

#[derive(Default)]
pub struct Palette {
    pub query: String,
    pub selected: usize,
}

impl Palette {
    /// Preserve the caller's order and never introduce unavailable commands.
    pub fn filtered(&self, available: &[Command]) -> Vec<Command> {
        let query = self.query.to_lowercase();
        available
            .iter()
            .copied()
            .filter(|command| {
                let searchable = format!(
                    "{} {} {} {} {} {}",
                    command.label(),
                    command.group(),
                    command.shortcut(),
                    command.aliases(),
                    command.localized_label(Language::Russian),
                    command.localized_group(Language::Russian)
                )
                .to_lowercase();
                query
                    .split_whitespace()
                    .all(|word| searchable.contains(word))
            })
            .collect()
    }

    /// `count` is the current filtered result count; Enter/Esc are handled by the caller.
    /// Editing appends at the end, Backspace removes one Unicode scalar, Ctrl+U clears.
    pub fn key(&mut self, key: KeyEvent, count: usize) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.selected = self.selected.min(count.saturating_sub(1));
        if key.modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        ) {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('u' | 'U' | 'г' | 'Г')) {
                self.query.clear();
                self.selected = 0;
            }
            // In particular, Ctrl+P remains reserved for the caller's palette toggle.
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = self.selected.saturating_add(1).min(count.saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                }
            }
            KeyCode::Char(c)
                if query_char(c)
                    && self.query.len().saturating_add(c.len_utf8()) <= MAX_QUERY_BYTES =>
            {
                self.query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    /// Reject the entire paste if it is unsafe or would exceed the byte limit.
    pub fn paste(&mut self, text: &str) {
        if !text.is_empty()
            && self.query.len().saturating_add(text.len()) <= MAX_QUERY_BYTES
            && text.chars().all(query_char)
        {
            self.query.push_str(text);
            self.selected = 0;
        }
    }
}

fn query_char(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn filters_bilingual_words_and_only_available_actions() {
        let commands = [Command::NewNote, Command::SaveNote, Command::Quit];
        let mut palette = Palette::default();
        assert_eq!(palette.filtered(&commands), commands);
        palette.paste("SAVE note");
        assert_eq!(palette.filtered(&commands), [Command::SaveNote]);
        palette.query = "СОЗДАТЬ заметка".into();
        assert_eq!(palette.filtered(&commands), [Command::NewNote]);
        palette.query = "Ctrl+S".into();
        assert_eq!(palette.filtered(&commands), [Command::SaveNote]);
        palette.query = "undo".into();
        assert!(palette.filtered(&commands).is_empty());
        palette.query = "powershell.exe -Command remove-item".into();
        assert!(palette.filtered(&commands).is_empty());
    }

    #[test]
    fn selection_is_bounded_even_when_count_shrinks_or_index_overflows() {
        let mut palette = Palette::default();
        palette.key(key(KeyCode::Down), 0);
        assert_eq!(palette.selected, 0);
        palette.key(key(KeyCode::Up), 0);
        palette.key(key(KeyCode::Down), 2);
        palette.key(key(KeyCode::Tab), 2);
        assert_eq!(palette.selected, 1);
        palette.selected = usize::MAX;
        palette.key(key(KeyCode::Down), usize::MAX);
        assert_eq!(palette.selected, usize::MAX - 1);
        palette.key(key(KeyCode::Down), 1);
        assert_eq!(palette.selected, 0);
        palette.key(key(KeyCode::End), 3);
        assert_eq!(palette.selected, 2);
        palette.key(key(KeyCode::BackTab), 3);
        assert_eq!(palette.selected, 1);
        palette.key(key(KeyCode::Home), 3);
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn editing_respects_utf8_byte_limit_and_resets_selection() {
        let mut palette = Palette::default();
        palette.paste(&"я".repeat(128));
        assert_eq!(palette.query.len(), MAX_QUERY_BYTES);
        palette.key(key(KeyCode::Char('界')), 3);
        assert_eq!(palette.query.len(), MAX_QUERY_BYTES);
        palette.selected = 2;
        palette.key(key(KeyCode::Backspace), 3);
        assert_eq!(palette.query.len(), 254);
        assert_eq!(palette.selected, 0);
        palette.key(key(KeyCode::Char('界')), 3);
        assert_eq!(palette.query.len(), 254);
        palette.key(key(KeyCode::Char('я')), 3);
        assert_eq!(palette.query.len(), MAX_QUERY_BYTES);
        palette.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 3);
        assert!(palette.query.is_empty());
    }

    #[test]
    fn unsafe_and_oversized_pastes_are_atomic() {
        let mut palette = Palette::default();
        palette.paste("find");
        palette.selected = 2;
        for text in [
            "\nq",
            "\r",
            "\t",
            "\x1b[31m",
            "\0",
            "a\u{2028}b",
            "\u{202e}",
        ] {
            palette.paste(text);
            assert_eq!(palette.query, "find");
            assert_eq!(palette.selected, 2);
        }
        palette.paste(&"a".repeat(253));
        assert_eq!(palette.query, "find");
        palette.paste(" заметка");
        assert_eq!(palette.query, "find заметка");
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn modified_keys_and_releases_do_not_become_query_text() {
        let mut palette = Palette::default();
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            palette.key(KeyEvent::new(KeyCode::Char('p'), modifiers), 3);
        }
        let mut release = key(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        palette.key(release, 3);
        palette.key(key(KeyCode::Char('\n')), 3);
        assert!(palette.query.is_empty());
        palette.paste("текст");
        palette.key(KeyEvent::new(KeyCode::Char('Г'), KeyModifiers::CONTROL), 3);
        assert!(palette.query.is_empty());
    }
}
