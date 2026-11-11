//! Символы интерфейса с запасным набором для классической консоли Windows.
//!
//! Старая консоль (conhost — окно PowerShell или cmd вне Windows Terminal) рисует
//! только то, что есть в её шрифте (Consolas, Lucida Console), и не подставляет
//! недостающее из других шрифтов. Брайлевский спиннер, ✓ ✗ ⚠ ▸ ▾, скруглённые углы
//! рамок и дробные блоки прогресс-бара там превращаются в квадратики. Для неё берём
//! символы из кодовой страницы 437 — они есть в любом консольном шрифте.

use std::sync::OnceLock;

use ratatui::widgets::BorderType;

pub struct Glyphs {
    pub spinner: &'static [&'static str],
    pub border: BorderType,
    /// Дробные блоки (▏▎▍…) в прогресс-барах.
    pub fine_gauge: bool,
    pub play: &'static str,
    pub back: &'static str,
    pub collapsed: &'static str,
    pub expanded: &'static str,
    pub ok: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    border: BorderType::Rounded,
    fine_gauge: true,
    play: "▶",
    back: "◀",
    collapsed: "▸",
    expanded: "▾",
    ok: "✓",
    fail: "✗",
    warn: "⚠",
};

const LEGACY: Glyphs = Glyphs {
    spinner: &["|", "/", "-", "\\"],
    border: BorderType::Plain,
    fine_gauge: false,
    play: "►",
    back: "◄",
    collapsed: "►",
    expanded: "▼",
    ok: "√",
    fail: "×",
    warn: "!",
};

pub fn glyphs() -> &'static Glyphs {
    static CHOICE: OnceLock<&'static Glyphs> = OnceLock::new();
    CHOICE.get_or_init(|| if legacy_console() { &LEGACY } else { &UNICODE })
}

/// `RUSTY_TUBE_GLYPHS=unicode|legacy` задаёт набор явно; иначе угадываем по окружению.
fn legacy_console() -> bool {
    match std::env::var("RUSTY_TUBE_GLYPHS").as_deref() {
        Ok("unicode") => return false,
        Ok("legacy") => return true,
        _ => {}
    }
    if !cfg!(windows) {
        return false;
    }
    // Windows Terminal, терминал VS Code, WezTerm, Alacritty, ConEmu рисуют всё сами
    // и подставляют недостающие символы из других шрифтов.
    const MODERN: [&str; 5] = [
        "WT_SESSION",
        "TERM_PROGRAM",
        "WEZTERM_PANE",
        "ALACRITTY_WINDOW_ID",
        "ConEmuPID",
    ];
    !MODERN.iter().any(|name| std::env::var_os(name).is_some())
}
