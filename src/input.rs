//! Однострочное поле ввода с курсором.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Default)]
pub struct Input {
    value: String,
    /// Позиция курсора в символах.
    cursor: usize,
}

impl Input {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn set(&mut self, value: impl Into<String>) {
        *self = Self::new(value);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map_or(self.value.len(), |(i, _)| i)
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            let at = self.byte_index(self.cursor);
            self.value.insert(at, c);
            self.cursor += 1;
        }
    }

    /// Обрабатывает клавиши редактирования. Возвращает `true`, если клавиша использована.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => self.clear(),
            KeyCode::Char(c) if !ctrl => self.insert_str(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_index(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Delete if self.cursor < self.value.chars().count() => {
                let at = self.byte_index(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Видимая часть строки шириной `width` и смещение курсора внутри неё.
    pub fn visible(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let start = self.cursor.saturating_sub(width - 1);
        let text = self.value.chars().skip(start).take(width).collect();
        (text, self.cursor - start)
    }
}

/// Ctrl+буква при русской раскладке → та же клавиша в латинице.
///
/// В классической консоли Windows (conhost) Ctrl+V приходит в программу как нажатие,
/// и crossterm переводит клавишу в символ по текущей раскладке: при русской выходит
/// Ctrl+м, и сочетания не срабатывают. Windows Terminal вставку обрабатывает сам.
pub fn latin_shortcut(mut key: KeyEvent) -> KeyEvent {
    const CYRILLIC: &str = "йцукенгшщзфывапролдячсмить";
    const LATIN: &str = "qwertyuiopasdfghjklzxcvbnm";
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
        && let Some(i) = c
            .to_lowercase()
            .next()
            .and_then(|c| CYRILLIC.chars().position(|r| r == c))
    {
        key.code = KeyCode::Char(LATIN.as_bytes()[i] as char);
    }
    key
}

/// Текст из системного буфера обмена.
pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), modifiers)
    }

    #[test]
    fn latin_shortcut_maps_russian_layout_with_ctrl() {
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(latin_shortcut(key('м', ctrl)).code, KeyCode::Char('v'));
        assert_eq!(latin_shortcut(key('М', ctrl)).code, KeyCode::Char('v'));
        assert_eq!(latin_shortcut(key('г', ctrl)).code, KeyCode::Char('u'));
        assert_eq!(latin_shortcut(key('щ', ctrl)).code, KeyCode::Char('o'));
        assert_eq!(latin_shortcut(key('v', ctrl)).code, KeyCode::Char('v'));
    }

    #[test]
    fn latin_shortcut_keeps_plain_typing() {
        assert_eq!(
            latin_shortcut(key('м', KeyModifiers::NONE)).code,
            KeyCode::Char('м')
        );
        assert_eq!(
            latin_shortcut(key('х', KeyModifiers::CONTROL)).code,
            KeyCode::Char('х')
        );
    }
}
