//! Работа со scoop: поиск, установка, проверка приложений и обновлений.
//!
//! Состояние приложений читается прямо из каталога scoop (`apps\<app>\current`,
//! `buckets\<bucket>\bucket\<app>.json`): это быстрее и надёжнее, чем разбирать
//! табличный вывод `scoop status`.
//!
//! Тот же интерфейс, что и у `brew.rs`: модуль подключается в `main.rs` под именем
//! `packages` в зависимости от платформы.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::worker::{self, Ctx};

/// Название менеджера пакетов для сообщений.
pub const MANAGER: &str = "Scoop";
/// Команда менеджера пакетов для подсказок вида «попробуйте вручную».
pub const CLI: &str = "scoop";

/// Что нужно для работы. deno — JS-рантайм, без которого современный yt-dlp
/// не может расшифровать подписи YouTube и отдаёт только часть форматов.
pub const REQUIRED_APPS: &[&str] = &["yt-dlp", "ffmpeg", "deno"];

#[derive(Debug, Clone)]
pub struct AppUpdate {
    pub name: String,
    pub installed: String,
    pub latest: String,
}

#[derive(Debug, Clone)]
pub struct Packages {
    root: PathBuf,
}

impl Packages {
    /// `$env:SCOOP` или `%USERPROFILE%\scoop` — так же, как ищет сам установщик.
    pub fn locate() -> Packages {
        let root = env::var_os("SCOOP")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| dirs::home_dir().map(|h| h.join("scoop")))
            .unwrap_or_else(|| PathBuf::from(r"C:\scoop"));
        Packages { root }
    }

    fn script(&self) -> Option<PathBuf> {
        [
            self.root.join(r"apps\scoop\current\bin\scoop.ps1"),
            self.root.join(r"shims\scoop.ps1"),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    pub fn is_installed(&self) -> bool {
        self.script().is_some()
    }

    /// Команда установки самого scoop.
    pub fn install_manager(&self) -> Result<Command, String> {
        Ok(install_command())
    }

    /// Где искали менеджер пакетов (для сообщения об ошибке).
    pub fn location(&self) -> String {
        self.root.display().to_string()
    }

    fn shims(&self) -> PathBuf {
        self.root.join("shims")
    }

    fn app_current(&self, app: &str) -> PathBuf {
        self.root.join("apps").join(app).join("current")
    }

    /// Служебный файл приложения (`manifest`/`install`). Scoop 0.6+ пишет его
    /// как `scoop-<name>.json`, более старые версии — как `<name>.json`;
    /// у приложений, поставленных разными версиями scoop, встречаются оба.
    fn app_meta(&self, app: &str, name: &str) -> Option<PathBuf> {
        let dir = self.app_current(app);
        [
            dir.join(format!("scoop-{name}.json")),
            dir.join(format!("{name}.json")),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    fn is_app_installed(&self, app: &str) -> bool {
        self.app_meta(app, "manifest").is_some()
    }

    /// Недостающие приложения. git нужен самому scoop для `scoop update`,
    /// поэтому ставим его, только если в системе git нет вообще.
    pub fn missing_apps(&self) -> Vec<String> {
        let mut missing: Vec<String> = Vec::new();
        if !self.git_available() {
            missing.push("git".into());
        }
        missing.extend(
            REQUIRED_APPS
                .iter()
                .filter(|app| !self.is_app_installed(app))
                .map(|app| app.to_string()),
        );
        missing
    }

    fn git_available(&self) -> bool {
        self.is_app_installed("git") || find_in_path("git.exe").is_some()
    }

    pub fn yt_dlp(&self) -> PathBuf {
        self.app_current("yt-dlp").join("yt-dlp.exe")
    }

    pub fn ffmpeg_dir(&self) -> PathBuf {
        self.app_current("ffmpeg").join("bin")
    }

    /// PATH для дочерних процессов: shims scoop в начале. Нужен, если scoop
    /// поставили только что и PATH текущего процесса ещё старый (yt-dlp ищет deno в PATH).
    pub fn path_env(&self) -> OsString {
        let mut paths = vec![self.shims()];
        if let Some(path) = env::var_os("PATH") {
            paths.extend(env::split_paths(&path));
        }
        env::join_paths(paths).unwrap_or_else(|_| env::var_os("PATH").unwrap_or_default())
    }

    pub fn install_command(&self, apps: &[String]) -> Command {
        self.command_with("install", apps)
    }

    pub fn upgrade_command(&self, apps: &[String]) -> Command {
        self.command_with("update", apps)
    }

    fn command_with(&self, verb: &str, apps: &[String]) -> Command {
        let mut args = vec![verb];
        args.extend(apps.iter().map(String::as_str));
        self.command(&args)
    }

    /// `scoop <args>` через PowerShell.
    fn command(&self, args: &[&str]) -> Command {
        let script = self
            .script()
            .unwrap_or_else(|| self.root.join(r"shims\scoop.ps1"));
        let mut cmd = powershell(&format!(
            "& {} {}; exit $LASTEXITCODE",
            ps_quote(&script.to_string_lossy()),
            args.iter()
                .map(|a| ps_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        cmd.env("PATH", self.path_env());
        cmd
    }

    fn installed_version(&self, app: &str) -> Option<String> {
        read_version(&self.app_meta(app, "manifest")?)
    }

    /// Версия из локальной копии bucket (актуальна после `scoop update`).
    fn latest_version(&self, app: &str) -> Option<String> {
        let install = read_json(&self.app_meta(app, "install")?)?;
        let bucket = install.get("bucket")?.as_str()?.to_string();
        let dir = self.root.join("buckets").join(bucket);
        let file = format!("{app}.json");
        [dir.join("bucket").join(&file), dir.join(&file)]
            .iter()
            .find_map(|p| read_version(p))
    }

    /// `scoop update`, затем сравнение версий с локальной копией bucket.
    pub fn check_updates(&self, ctx: &Ctx) -> Result<Vec<AppUpdate>, String> {
        ctx.step("Обновление списков пакетов scoop (Esc — пропустить)");
        let status = worker::run(ctx, self.command(&["update"]), |line, _| ctx.log(line))?;
        if !status.success() {
            ctx.log("scoop update завершился с ошибкой — сравниваю с локальными данными");
        }
        Ok(self.outdated())
    }

    fn outdated(&self) -> Vec<AppUpdate> {
        REQUIRED_APPS
            .iter()
            .filter_map(|app| {
                let installed = self.installed_version(app)?;
                let latest = self.latest_version(app)?;
                (installed != latest).then(|| AppUpdate {
                    name: app.to_string(),
                    installed,
                    latest,
                })
            })
            .collect()
    }
}

/// Официальный установщик scoop (https://get.scoop.sh). Под администратором
/// установщик требует флаг `-RunAsAdmin`. Политика выполнения RemoteSigned для
/// текущего пользователя нужна, чтобы scoop потом работал и из обычной консоли.
fn install_command() -> Command {
    powershell(
        r#"
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$policy = Get-ExecutionPolicy -Scope CurrentUser
if ($policy -eq 'Undefined' -or $policy -eq 'Restricted' -or $policy -eq 'AllSigned') {
    Write-Output 'Политика выполнения для текущего пользователя: RemoteSigned'
    Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser -Force
}
$installer = Join-Path $env:TEMP 'install-scoop.ps1'
Write-Output 'Загрузка установщика scoop...'
Invoke-RestMethod -Uri 'https://get.scoop.sh' -OutFile $installer
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    & $installer -RunAsAdmin
} else {
    & $installer
}
Remove-Item $installer -ErrorAction SilentlyContinue
"#,
    )
}

fn powershell(script: &str) -> Command {
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &format!(
            "$ProgressPreference = 'SilentlyContinue'; \
             [Console]::OutputEncoding = [Text.Encoding]::UTF8; {script}"
        ),
    ]);
    cmd
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

fn read_version(path: &Path) -> Option<String> {
    Some(read_json(path)?.get("version")?.as_str()?.to_string())
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}
