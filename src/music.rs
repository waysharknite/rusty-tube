//! Поиск песен в YouTube Music через yt-dlp.
//!
//! Быстрый режим (`--flat-playlist`) отдаёт только id и название, поэтому каждый
//! результат извлекается полностью (около 2 с на трек). yt-dlp печатает результаты
//! по одному, и список заполняется по мере поступления.

use std::{ffi::OsString, path::PathBuf, process::Command};

use serde::Deserialize;

use crate::{
    config::{self, Browser, Config},
    worker::{self, Ctx, Event, Outcome},
};

/// Сколько результатов запрашивать.
const LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Track {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

impl Track {
    pub fn url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.id)
    }

    pub fn name(&self) -> &str {
        self.track.as_deref().unwrap_or(&self.title)
    }

    pub fn artist(&self) -> &str {
        self.artist.as_deref().unwrap_or("")
    }

    pub fn duration(&self) -> String {
        match self.duration {
            Some(d) if d > 0.0 => {
                let secs = d as u64;
                format!("{}:{:02}", secs / 60, secs % 60)
            }
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub config: Config,
    pub yt_dlp: PathBuf,
    pub path_env: OsString,
}

/// Ссылка на поиск в разделе «Песни».
pub fn search_url(query: &str) -> String {
    format!(
        "https://music.youtube.com/search?q={}#songs",
        percent_encode(query.trim())
    )
}

/// Ссылка на альбом YT Music (`/browse/MPREb…`) — yt-dlp раскрывает её как плейлист.
pub fn is_album(url: &str) -> bool {
    url.contains("music.youtube.com/browse/")
}

pub fn run(ctx: &Ctx, req: SearchRequest) -> Result<Outcome, String> {
    ctx.step(format!("Поиск: {}", req.query));
    let cookies = config::cookies_path();
    let use_cookies = req.config.browser != Browser::None && cookies.is_file();

    let mut cmd = Command::new(&req.yt_dlp);
    cmd.env("PATH", &req.path_env)
        .arg("--ignore-config")
        .args(["--skip-download", "--ignore-errors", "--no-warnings"])
        .args(["--color", "never"])
        .args(["-I", &format!("1:{LIMIT}")])
        .args(["--print", "%(.{id,title,track,artist,album,duration})j"]);
    if use_cookies {
        cmd.arg("--cookies").arg(&cookies);
    }
    if let Some(proxy) = req.config.proxy_url() {
        cmd.args(["--proxy", &proxy]);
    }
    cmd.arg("--").arg(search_url(&req.query));

    let mut found = 0;
    let mut error = None;
    let status = worker::run(ctx, cmd, |line, _| {
        if let Some(track) = parse_line(line) {
            found += 1;
            ctx.send(Event::Track(track));
        } else if let Some(msg) = line.strip_prefix("ERROR: ") {
            error.get_or_insert_with(|| msg.to_string());
        }
    })?;
    if found == 0 && !status.success() {
        return Err(error.unwrap_or_else(|| {
            format!("yt-dlp завершился с кодом {}", status.code().unwrap_or(-1))
        }));
    }
    Ok(Outcome::Done)
}

fn parse_line(line: &str) -> Option<Track> {
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Track>(line)
        .ok()
        .filter(|t| !t.id.is_empty())
}

/// Кодирование строки запроса для URL (UTF-8, всё кроме безопасных символов — `%XX`).
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_search_url() {
        assert_eq!(
            search_url(" Кино & Цой "),
            "https://music.youtube.com/search?q=%D0%9A%D0%B8%D0%BD%D0%BE+%26+%D0%A6%D0%BE%D0%B9#songs"
        );
    }

    #[test]
    fn parses_print_line() {
        let line = r#"{"id": "9RfVp-GhKfs", "title": "Creep", "track": "Creep", "artist": "Radiohead", "album": null, "duration": 239}"#;
        let track = parse_line(line).unwrap();
        assert_eq!(track.name(), "Creep");
        assert_eq!(track.artist(), "Radiohead");
        assert_eq!(track.album, None);
        assert_eq!(track.duration(), "3:59");
        assert_eq!(track.url(), "https://music.youtube.com/watch?v=9RfVp-GhKfs");

        // У не-музыкальных роликов нет track/artist — берём title.
        let track = parse_line(r#"{"id": "x", "title": "Video", "duration": null}"#).unwrap();
        assert_eq!(track.name(), "Video");
        assert_eq!(track.duration(), "");

        assert!(parse_line("[youtube] x: Downloading webpage").is_none());
    }

    #[test]
    fn detects_album() {
        assert!(is_album(
            "https://music.youtube.com/browse/MPREb_yXhSI4FCUo6"
        ));
        assert!(!is_album("https://music.youtube.com/watch?v=abc"));
    }
}
