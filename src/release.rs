//! Проверка новой версии программы на GitHub.
//!
//! Запрос идёт через системный `curl` (есть в Windows 10+ и macOS), чтобы не тянуть
//! в программу HTTP-клиент. Любая ошибка — нет сети, нет curl, лимит GitHub —
//! просто означает «уведомления не будет».

use std::{
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
};

use serde::Deserialize;

const LATEST_URL: &str = "https://api.github.com/repos/waysharknite/rusty-tube/releases/latest";

/// Последний опубликованный релиз.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Тег как есть, например `v0.2.0`.
    pub tag: String,
    /// Страница релиза, откуда скачивается сборка.
    pub url: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
}

/// Запускает проверку в фоне. В канал придёт релиз, только если он новее текущей
/// версии и новее пропущенной пользователем (`skipped`).
pub fn check_in_background(skipped: Option<String>) -> Receiver<Release> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Some(release) = fetch_latest()
            && is_newer(&release.tag, env!("CARGO_PKG_VERSION"))
            && skipped.as_deref().is_none_or(|s| is_newer(&release.tag, s))
        {
            let _ = tx.send(release);
        }
    });
    rx
}

fn fetch_latest() -> Option<Release> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--silent",
        "--fail",
        "--location",
        "--max-time",
        "10",
        "--header",
        "Accept: application/vnd.github+json",
        LATEST_URL,
    ])
    .stdin(Stdio::null())
    .stderr(Stdio::null());
    crate::worker::isolate(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&output.stdout)
}

fn parse(json: &[u8]) -> Option<Release> {
    let api: ApiRelease = serde_json::from_slice(json).ok()?;
    Some(Release {
        tag: api.tag_name,
        url: api.html_url,
    })
}

/// `v1.2.3`, `1.2`, `1.2.3-beta` → (1, 2, 3). Нечисловой хвост компонента отбрасывается.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = text.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// `candidate` строго новее `current`. Нераспознанная версия — не новее.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Открывает страницу в браузере по умолчанию.
pub fn open_in_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    } else if cfg!(target_os = "macos") {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    } else {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.3"), Some((1, 3, 0)));
        assert_eq!(parse_version("v1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_version("latest"), None);

        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        // Локальная сборка новее последнего релиза — не предлагать «обновиться» назад.
        assert!(!is_newer("1.0.0", "1.1.0"));
        assert!(!is_newer("v1.0.0", "1.1.0"));
        assert!(is_newer("1.2.0", "1.1.0"));
    }

    #[test]
    fn parses_github_response() {
        let json = br#"{"tag_name":"v0.2.0","html_url":"https://github.com/waysharknite/rusty-tube/releases/tag/v0.2.0","assets":[]}"#;
        assert_eq!(
            parse(json),
            Some(Release {
                tag: "v0.2.0".into(),
                url: "https://github.com/waysharknite/rusty-tube/releases/tag/v0.2.0".into(),
            })
        );
        assert_eq!(parse(b"{\"message\":\"Not Found\"}"), None);
    }
}
