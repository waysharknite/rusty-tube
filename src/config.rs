//! Пользовательские настройки и пути к файлам приложения.

use std::{fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "rusty-tube";

/// Браузер, из профиля которого берутся cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Browser {
    #[default]
    Chrome,
    Brave,
    Firefox,
    /// Только macOS.
    Safari,
    Yandex,
    None,
}

impl Browser {
    /// Браузеры, доступные на текущей платформе.
    pub const ALL: &[Browser] = if cfg!(target_os = "macos") {
        &[
            Browser::Chrome,
            Browser::Brave,
            Browser::Firefox,
            Browser::Safari,
            Browser::Yandex,
            Browser::None,
        ]
    } else {
        &[
            Browser::Chrome,
            Browser::Brave,
            Browser::Firefox,
            Browser::Yandex,
            Browser::None,
        ]
    };

    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Google Chrome",
            Browser::Brave => "Brave",
            Browser::Firefox => "Mozilla Firefox",
            Browser::Safari => "Safari",
            Browser::Yandex => "Яндекс Браузер",
            Browser::None => "Не использовать",
        }
    }

    /// Папка данных браузера (подсказка в настройках). Под подсказку в окне настроек
    /// остаётся 50 символов, поэтому полный путь до файла cookie не пишем.
    #[cfg(windows)]
    pub fn location_hint(self) -> &'static str {
        match self {
            Browser::Chrome => r"%LOCALAPPDATA%\Google\Chrome\User Data",
            Browser::Brave => r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser",
            Browser::Firefox => r"%APPDATA%\Mozilla\Firefox\Profiles",
            Browser::Yandex => r"%LOCALAPPDATA%\Yandex\YandexBrowser",
            Browser::Safari => "",
            Browser::None => "Загрузка без cookie",
        }
    }

    /// Папка данных браузера (подсказка в настройках, не длиннее 50 символов).
    #[cfg(target_os = "macos")]
    pub fn location_hint(self) -> &'static str {
        match self {
            Browser::Chrome => "~/Library/Application Support/Google/Chrome",
            Browser::Brave => "~/Library/Application Support/BraveSoftware",
            Browser::Firefox => "~/Library/Application Support/Firefox",
            Browser::Safari => "Нужен «Полный доступ к диску» для Терминала",
            Browser::Yandex => "~/Library/Application Support/Yandex",
            Browser::None => "Загрузка без cookie",
        }
    }

    /// Где лежит база cookie браузера (для подсказки в настройках).
    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn location_hint(self) -> &'static str {
        match self {
            Browser::Chrome => "~/.config/google-chrome/Default",
            Browser::Brave => "~/.config/BraveSoftware/Brave-Browser/Default",
            Browser::Firefox => "~/.mozilla/firefox/<профиль>",
            Browser::Yandex => "~/.config/yandex-browser/Default",
            Browser::Safari => "",
            Browser::None => "Загрузка без cookie",
        }
    }

    /// Значение для `yt-dlp --cookies-from-browser`.
    ///
    /// Яндекс Браузер — Chromium, но yt-dlp не знает его по имени, поэтому
    /// передаём движок `chrome` и явный путь к профилю. yt-dlp сам найдёт
    /// `Network\Cookies` внутри профиля и `Local State` уровнем выше.
    pub fn cookies_spec(self) -> Option<String> {
        match self {
            Browser::Chrome => Some("chrome".into()),
            Browser::Brave => Some("brave".into()),
            Browser::Firefox => Some("firefox".into()),
            Browser::Safari => Some("safari".into()),
            Browser::Yandex => Some(format!("chrome:{}", yandex_profile()?.display())),
            Browser::None => None,
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
}

/// Профиль Default Яндекс Браузера.
fn yandex_profile() -> Option<PathBuf> {
    if cfg!(windows) {
        Some(dirs::data_local_dir()?.join(r"Yandex\YandexBrowser\User Data\Default"))
    } else if cfg!(target_os = "macos") {
        // ~/Library/Application Support
        Some(dirs::data_dir()?.join("Yandex/YandexBrowser/Default"))
    } else {
        Some(dirs::config_dir()?.join("yandex-browser/Default"))
    }
}

/// Ограничение качества видео.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    #[default]
    Best,
    P2160,
    P1440,
    P1080,
    P720,
    P480,
    P360,
}

impl Quality {
    pub const ALL: [Quality; 7] = [
        Quality::Best,
        Quality::P2160,
        Quality::P1440,
        Quality::P1080,
        Quality::P720,
        Quality::P480,
        Quality::P360,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Best => "Лучшее доступное",
            Quality::P2160 => "До 2160p (4K)",
            Quality::P1440 => "До 1440p",
            Quality::P1080 => "До 1080p",
            Quality::P720 => "До 720p",
            Quality::P480 => "До 480p",
            Quality::P360 => "До 360p",
        }
    }

    /// Максимальная высота кадра или `None`, если без ограничений.
    pub fn max_height(self) -> Option<u32> {
        match self {
            Quality::Best => None,
            Quality::P2160 => Some(2160),
            Quality::P1440 => Some(1440),
            Quality::P1080 => Some(1080),
            Quality::P720 => Some(720),
            Quality::P480 => Some(480),
            Quality::P360 => Some(360),
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|q| *q == self).unwrap_or(0)
    }
}

/// Формат файлов в режиме музыки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioFormat {
    /// Opus как есть, без перекодирования: лучшее качество, которое отдаёт YouTube.
    #[default]
    Opus,
    /// AAC в m4a: без перекодирования, если есть AAC-поток.
    M4a,
    /// MP3 VBR V0: перекодирование, зато играет везде.
    Mp3,
}

impl AudioFormat {
    pub const ALL: [AudioFormat; 3] = [AudioFormat::Opus, AudioFormat::M4a, AudioFormat::Mp3];

    pub fn label(self) -> &'static str {
        match self {
            AudioFormat::Opus => "Opus (оригинал)",
            AudioFormat::M4a => "M4A (AAC)",
            AudioFormat::Mp3 => "MP3 (перекодир.)",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub download_dir: PathBuf,
    /// Папка для режима музыки.
    pub music_dir: PathBuf,
    pub browser: Browser,
    pub quality: Quality,
    pub audio_format: AudioFormat,
    /// Порт локального SOCKS5-прокси (VPN-клиента) на 127.0.0.1; `None` — без прокси.
    pub proxy_port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            music_dir: default_music_dir(),
            browser: Browser::default(),
            quality: Quality::default(),
            audio_format: AudioFormat::default(),
            proxy_port: None,
        }
    }
}

impl Config {
    /// Загружает настройки. `None` — файла нет (первый запуск) или он испорчен.
    pub fn load() -> Option<Config> {
        let text = fs::read_to_string(config_path()?).ok()?;
        toml::from_str(&text).ok()
    }

    /// Значение для `yt-dlp --proxy`. `socks5h` — DNS тоже резолвится через прокси,
    /// иначе заблокированные домены не находятся.
    pub fn proxy_url(&self) -> Option<String> {
        self.proxy_port
            .map(|port| format!("socks5h://127.0.0.1:{port}"))
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("не найден каталог AppData"))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, text)
    }
}

/// Пользовательская папка «Видео» (FOLDERID_Videos, на macOS ~/Movies).
pub fn default_download_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Пользовательская папка «Музыка» (FOLDERID_Music, на macOS ~/Music).
pub fn default_music_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%APPDATA%\rusty-tube\config.toml`, на macOS — `~/Library/Application Support/rusty-tube/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("config.toml"))
}

/// `%LOCALAPPDATA%\rusty-tube` (на macOS — `~/Library/Application Support/rusty-tube`):
/// сюда складывается экспортированный cookies.txt.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

pub fn cookies_path() -> PathBuf {
    data_dir().join("cookies.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_port_roundtrip() {
        let mut config = Config {
            proxy_port: Some(1080),
            ..Config::default()
        };
        assert_eq!(
            config.proxy_url().as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&text).unwrap().proxy_port,
            Some(1080)
        );

        // Без прокси поле не пишется, а старые конфиги без него читаются.
        config.proxy_port = None;
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("proxy_port"));
        assert_eq!(toml::from_str::<Config>(&text).unwrap().proxy_port, None);
        assert_eq!(config.proxy_url(), None);
    }

    #[test]
    fn brave_roundtrip() {
        let config = Config {
            browser: Browser::Brave,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("browser = \"brave\""));
        assert_eq!(
            toml::from_str::<Config>(&text).unwrap().browser,
            Browser::Brave
        );
        assert_eq!(Browser::Brave.cookies_spec().as_deref(), Some("brave"));
        assert!(Browser::ALL.contains(&Browser::Brave));
    }

    #[test]
    fn location_hints_fit_settings() {
        // Окно настроек 78 колонок: рамка, поля и отступ 22 оставляют под подсказку 50.
        for browser in Browser::ALL {
            let hint = browser.location_hint();
            assert!(hint.chars().count() <= 50, "{browser:?}: {hint}");
        }
    }

    #[test]
    fn old_config_gets_music_defaults() {
        let text = "download_dir = 'D:\\Видео'\nbrowser = \"yandex\"\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.audio_format, AudioFormat::Opus);
        assert_eq!(config.music_dir, default_music_dir());
        assert_eq!(config.browser, Browser::Yandex);
    }
}
