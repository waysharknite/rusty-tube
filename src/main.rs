//! rusty-tube — TUI-загрузчик видео с YouTube на базе yt-dlp и ffmpeg.
//! Зависимости ставятся через scoop (Windows) или Homebrew (macOS).

mod app;
mod config;
mod download;
mod input;
mod install;
mod music;
mod release;
mod socks_bridge;
mod subtitles;
mod theme;
mod timecode;
mod ui;
mod worker;

/// Менеджер пакетов платформы: оба модуля дают одинаковый интерфейс.
#[cfg(windows)]
#[path = "scoop.rs"]
mod packages;
#[cfg(not(windows))]
#[path = "brew.rs"]
mod packages;

use std::io::stdout;

use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

fn main() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    // Вставка Cmd+V в терминалах macOS приходит одним событием Paste, а не нажатиями
    // клавиш (иначе перевод строки во вставке сработал бы как Enter). В консоли Windows
    // вставка и так приходит клавишами.
    #[cfg(not(windows))]
    let _ = execute!(stdout(), ratatui::crossterm::event::EnableBracketedPaste);
    let result = app::App::new().run(&mut terminal);
    #[cfg(not(windows))]
    let _ = execute!(stdout(), ratatui::crossterm::event::DisableBracketedPaste);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(message) = result? {
        eprintln!("{message}");
        std::process::exit(1);
    }
    Ok(())
}
