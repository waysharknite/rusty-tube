//! Работа с Homebrew (macOS, Linux): поиск, установка, проверка приложений и обновлений.
//!
//! Тот же интерфейс, что и у `scoop.rs`: модуль подключается в `main.rs` под именем
//! `packages` в зависимости от платформы.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::worker::{self, Ctx};

/// Название менеджера пакетов для сообщений.
pub const MANAGER: &str = "Homebrew";
/// Команда менеджера пакетов для подсказок вида «попробуйте вручную».
pub const CLI: &str = "brew";

/// Что нужно для работы. deno — JS-рантайм, без которого современный yt-dlp
/// не может расшифровать подписи YouTube и отдаёт только часть форматов.
pub const REQUIRED_APPS: &[&str] = &["yt-dlp", "ffmpeg", "deno"];

/// Официальная команда установки Homebrew.
const BREW_INSTALL: &str = r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#;

#[derive(Debug, Clone)]
pub struct AppUpdate {
    pub name: String,
    pub installed: String,
    pub latest: String,
}

#[derive(Debug, Clone)]
pub struct Packages {
    /// Путь к исполняемому файлу `brew`, `None` — Homebrew не установлен.
    brew: Option<PathBuf>,
}

impl Packages {
    /// Ищем `brew` в стандартных местах, а потом в PATH. Программу могут запустить
    /// из окружения без `brew shellenv` (например, сразу после установки Homebrew),
    /// поэтому только на PATH полагаться нельзя.
    pub fn locate() -> Packages {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(prefix) = env::var_os("HOMEBREW_PREFIX").filter(|p| !p.is_empty()) {
            candidates.push(PathBuf::from(prefix).join("bin/brew"));
        }
        candidates.extend(
            [
                "/opt/homebrew/bin/brew",              // macOS, Apple Silicon
                "/usr/local/bin/brew",                 // macOS, Intel
                "/home/linuxbrew/.linuxbrew/bin/brew", // Linux
            ]
            .map(PathBuf::from),
        );
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".linuxbrew/bin/brew"));
        }
        let brew = candidates
            .into_iter()
            .find(|p| p.is_file())
            .or_else(|| find_in_path("brew"));
        Packages { brew }
    }

    pub fn is_installed(&self) -> bool {
        self.brew.is_some()
    }

    /// Homebrew сам из программы не ставится: установщику нужен пароль sudo,
    /// а TUI не даёт его ввести. Поэтому показываем команду для Терминала.
    pub fn install_manager(&self) -> Result<Command, String> {
        Err(format!(
            "Homebrew не найден. Он нужен, чтобы поставить yt-dlp, ffmpeg и deno.\n\
             Установите его командой в Терминале:\n\n  {BREW_INSTALL}\n\n\
             и запустите программу снова."
        ))
    }

    /// Где искали менеджер пакетов (для сообщения об ошибке).
    pub fn location(&self) -> String {
        self.brew
            .as_deref()
            .map_or_else(|| "PATH".into(), |p| p.display().to_string())
    }

    /// Префикс Homebrew: `brew` лежит в `<prefix>/bin/brew`.
    fn prefix(&self) -> Option<&Path> {
        self.brew.as_deref()?.parent()?.parent()
    }

    fn bin(&self) -> PathBuf {
        self.prefix()
            .map_or_else(|| PathBuf::from("/usr/local/bin"), |p| p.join("bin"))
    }

    /// У установленной формулы есть ссылка `<prefix>/opt/<формула>`.
    fn is_app_installed(&self, app: &str) -> bool {
        self.prefix()
            .is_some_and(|p| p.join("opt").join(app).exists())
    }

    pub fn missing_apps(&self) -> Vec<String> {
        REQUIRED_APPS
            .iter()
            .filter(|app| !self.is_app_installed(app))
            .map(|app| app.to_string())
            .collect()
    }

    pub fn yt_dlp(&self) -> PathBuf {
        self.bin().join("yt-dlp")
    }

    pub fn ffmpeg_dir(&self) -> PathBuf {
        self.bin()
    }

    /// PATH для дочерних процессов: `<prefix>/bin` в начале (yt-dlp ищет deno в PATH).
    pub fn path_env(&self) -> OsString {
        let mut paths = vec![self.bin()];
        if let Some(path) = env::var_os("PATH") {
            paths.extend(env::split_paths(&path));
        }
        env::join_paths(paths).unwrap_or_else(|_| env::var_os("PATH").unwrap_or_default())
    }

    fn command(&self, args: &[&str]) -> Command {
        let brew = self.brew.clone().unwrap_or_else(|| PathBuf::from(CLI));
        let mut cmd = Command::new(brew);
        cmd.args(args)
            .env("PATH", self.path_env())
            .env("HOMEBREW_NO_COLOR", "1")
            .env("HOMEBREW_NO_EMOJI", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1");
        cmd
    }

    pub fn install_command(&self, apps: &[String]) -> Command {
        let mut cmd = self.command(&["install"]);
        cmd.args(apps);
        cmd
    }

    pub fn upgrade_command(&self, apps: &[String]) -> Command {
        let mut cmd = self.command(&["upgrade"]);
        cmd.args(apps);
        cmd
    }

    /// `brew update`, затем `brew outdated` по нужным формулам.
    pub fn check_updates(&self, ctx: &Ctx) -> Result<Vec<AppUpdate>, String> {
        ctx.step("Обновление списков пакетов Homebrew (Esc — пропустить)");
        let status = worker::run(ctx, self.command(&["update"]), |line, _| ctx.log(line))?;
        if !status.success() {
            ctx.log("brew update завершился с ошибкой — сравниваю с локальными данными");
        }

        ctx.step("Проверка версий");
        let mut cmd = self.command(&["outdated", "--json=v2", "--formula"]);
        cmd.args(REQUIRED_APPS).env("HOMEBREW_NO_AUTO_UPDATE", "1");
        let mut json = String::new();
        worker::run(ctx, cmd, |line, is_err| {
            if is_err {
                ctx.log(line);
            } else {
                json.push_str(line);
                json.push('\n');
            }
        })?;
        parse_outdated(&json)
    }
}

#[derive(Deserialize)]
struct Outdated {
    #[serde(default)]
    formulae: Vec<OutdatedFormula>,
}

#[derive(Deserialize)]
struct OutdatedFormula {
    name: String,
    #[serde(default)]
    installed_versions: Vec<String>,
    current_version: String,
}

fn parse_outdated(json: &str) -> Result<Vec<AppUpdate>, String> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    let outdated: Outdated = serde_json::from_str(json)
        .map_err(|e| format!("не удалось разобрать вывод brew outdated: {e}"))?;
    Ok(outdated
        .formulae
        .into_iter()
        .map(|f| AppUpdate {
            installed: f.installed_versions.last().cloned().unwrap_or_default(),
            name: f.name,
            latest: f.current_version,
        })
        .collect())
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_outdated_json() {
        let json = r#"{
  "formulae": [
    {
      "name": "yt-dlp",
      "installed_versions": ["2026.09.01"],
      "current_version": "2026.11.03",
      "pinned": false,
      "pinned_version": null
    }
  ],
  "casks": []
}"#;
        let updates = parse_outdated(json).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "yt-dlp");
        assert_eq!(updates[0].installed, "2026.09.01");
        assert_eq!(updates[0].latest, "2026.11.03");

        assert!(parse_outdated("").unwrap().is_empty());
        assert!(
            parse_outdated(r#"{"formulae": [], "casks": []}"#)
                .unwrap()
                .is_empty()
        );
    }
}
