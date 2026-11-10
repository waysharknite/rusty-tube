//! Палитры цветовых тем.
//!
//! Все цвета — из 256-цветной палитры (`Color::Indexed`), а не базовые 16 ANSI:
//! - базовые цвета переопределяет тема терминала (в Solarized «чёрный» — синевато-серый);
//! - Terminal.app и iTerm2 по умолчанию рисуют жирный текст цветов 0–7 их яркими
//!   вариантами, и жирный чёрный на красной кнопке становится серым.
//!
//! Индексы 16–255 терминалы не трогают, а 256 цветов есть и в Terminal.app,
//! и в Windows Terminal, и в conhost Windows 10+ (truecolor в Terminal.app — нет).

use ratatui::style::{Color, Style};

use crate::config::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Фон и текст всего экрана; пустой стиль — как в терминале.
    pub base: Style,
    /// Основной текст там, где фон задаёт приложение (полосы, шкалы).
    pub text: Color,
    /// Фон верхней и нижней полос и незаполненной части шкал.
    pub bar: Color,
    /// Акцент: рамка в фокусе, активная кнопка, выбранная строка.
    pub accent: Color,
    /// Текст на акцентном фоне.
    pub on_accent: Color,
    /// Второстепенный текст: подсказки, неактивные рамки.
    pub dim: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
    /// Шкала прогресса плейлиста.
    pub info: Color,
    /// Фон кнопок и полей без фокуса.
    pub control: Color,
    pub on_control: Color,
}

const fn c(index: u8) -> Color {
    Color::Indexed(index)
}

impl Theme {
    pub fn palette(self) -> Palette {
        match self {
            // Прежний вид, но с цветами, которые не зависят от настроек терминала.
            Theme::Classic => Palette {
                base: Style::new(),
                text: c(231),
                bar: c(16),
                accent: c(203),
                on_accent: c(16),
                dim: c(245),
                ok: c(77),
                err: c(196),
                warn: c(220),
                info: c(80),
                control: c(239),
                on_control: c(231),
            },
            // Чистый чёрный фон, белый текст, жёлтый акцент, светлые подсказки.
            Theme::Contrast => Palette {
                base: Style::new().fg(c(231)).bg(c(16)),
                text: c(231),
                bar: c(235),
                accent: c(226),
                on_accent: c(16),
                dim: c(250),
                ok: c(46),
                err: c(203),
                warn: c(214),
                info: c(51),
                control: c(240),
                on_control: c(231),
            },
            Theme::Blue => Palette {
                base: Style::new(),
                text: c(231),
                bar: c(16),
                accent: c(75),
                on_accent: c(16),
                dim: c(245),
                ok: c(77),
                err: c(203),
                warn: c(220),
                info: c(141),
                control: c(239),
                on_control: c(231),
            },
            // Для светлого профиля терминала (в Terminal.app он по умолчанию).
            Theme::Light => Palette {
                base: Style::new().fg(c(235)).bg(c(255)),
                text: c(235),
                bar: c(252),
                accent: c(160),
                on_accent: c(231),
                dim: c(242),
                ok: c(28),
                err: c(160),
                warn: c(130),
                info: c(25),
                control: c(250),
                on_control: c(16),
            },
        }
    }
}
