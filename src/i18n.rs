//! Interface language only: note contents, filenames and link labels are never translated.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ru")]
    Russian,
}

impl Language {
    pub fn text<'a>(self, english: &'a str, russian: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Russian => russian,
        }
    }
    pub fn error(self, detail: &str) -> String {
        if self == Self::English {
            return detail.into();
        }
        match detail {
            "Cannot save: note changed on disk; reload it before saving" => {
                "Заметка изменена на диске; перечитайте её перед сохранением".into()
            }
            "Document path changed; load or create the destination instead" => {
                "Путь заметки изменён; откройте или создайте нужный файл".into()
            }
            "Note is not valid UTF-8" => "Заметка не является корректным UTF-8".into(),
            "Note exceeds the 2 MiB limit (including BOM and encoded newlines)" => {
                "Заметка превышает лимит 2 МиБ (включая BOM и окончания строк)".into()
            }
            "Cannot save: note was deleted, moved, or is no longer accessible" => {
                "Заметка удалена, перемещена или недоступна".into()
            }
            "Cannot save: note was deleted, changed, or is no longer readable" => {
                "Заметка удалена, изменена или больше не читается".into()
            }
            "Cannot save: note was deleted or its path changed" => {
                "Заметка удалена или её путь изменён".into()
            }
            _ => detail.into(), // OS and path-specific diagnostics retain their original detail.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_to_english_and_preserves_unrecognized_error_data() {
        assert_eq!(Language::default(), Language::English);
        assert_eq!(Language::Russian.text("Save", "Сохранить"), "Сохранить");
        assert_eq!(Language::Russian.error("user-file.md"), "user-file.md");
    }
}
