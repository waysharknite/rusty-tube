//! Загрузка через yt-dlp: обновление cookie, запуск, разбор вывода.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    config::{self, AudioFormat, Browser, Config},
    worker::{self, Ctx, Event, Outcome, Progress},
};

/// Префикс строк прогресса, которые формирует наш `--progress-template`.
const PROGRESS_TAG: &str = "[rt]";

/// Что качаем: видео или только звук (режим музыки).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

/// Обрезка обложки до квадрата по центру: превью YouTube — 16:9 с полями по бокам.
/// Аргументы ffmpeg разбираются yt-dlp как shell-строка, поэтому фильтр в кавычках.
const SQUARE_COVER: &str = concat!(
    "ThumbnailsConvertor+ffmpeg_o:-c:v mjpeg -qmin 1 -qscale:v 1 ",
    r#"-vf "crop='min(iw,ih)':'min(iw,ih)'""#,
);

#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    pub kind: Kind,
    /// Качать весь плейлист (`--yes-playlist`) или только одно видео.
    pub playlist: bool,
    pub config: Config,
    pub yt_dlp: PathBuf,
    pub ffmpeg_dir: PathBuf,
    pub path_env: OsString,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadSummary {
    pub files: Vec<String>,
    pub errors: Vec<String>,
}

/// Ссылка на плейлист: отдельная страница плейлиста или видео с параметром `list=`.
pub fn is_playlist(url: &str) -> bool {
    url.contains("/playlist") || url.contains("list=")
}

/// Ссылка на конкретное видео внутри плейлиста (`watch?v=…&list=…`, `youtu.be/…?list=…`):
/// можно скачать и весь плейлист, и только это видео.
pub fn is_video_in_playlist(url: &str) -> bool {
    url.contains("list=")
        && (url.contains("?v=")
            || url.contains("&v=")
            || url.contains("youtu.be/")
            || url.contains("/shorts/"))
}

/// Приводит ввод к URL. `None` — это явно не ссылка.
pub fn normalize_url(input: &str) -> Option<String> {
    let s = input.trim().trim_matches(['"', '\'', '<', '>']);
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        return Some(s.to_string());
    }
    let lower = s.to_lowercase();
    let known = [
        "youtube.com/",
        "youtu.be/",
        "www.youtube.com/",
        "m.youtube.com/",
        "music.youtube.com/",
    ];
    known
        .iter()
        .any(|p| lower.starts_with(p))
        .then(|| format!("https://{s}"))
}

pub fn run(ctx: &Ctx, req: Request) -> Result<Outcome, String> {
    // Cookie экспортируются из браузера один раз при запуске программы (`update_cookies`),
    // здесь используется уже сохранённый файл.
    let cookies = config::cookies_path();
    let use_cookies = req.config.browser != Browser::None && cookies.is_file();
    if req.config.browser != Browser::None && !use_cookies {
        ctx.log("Файл cookie не найден — загрузка без cookie (cookie обновляются при запуске программы)");
    }

    ctx.step("Получение информации");
    let cmd = download_command(&req, req.playlist, use_cookies.then_some(cookies.as_path()));
    let mut parser = OutputParser::new(ctx, req.kind);
    let status = worker::run(ctx, cmd, |line, is_err| parser.feed(line, is_err))?;
    let summary = parser.finish();

    if summary.files.is_empty() && !status.success() {
        let reason = summary.errors.last().cloned().unwrap_or_else(|| {
            format!("yt-dlp завершился с кодом {}", status.code().unwrap_or(-1))
        });
        return Err(reason);
    }
    ctx.step("Готово");
    ctx.send(Event::Progress(Progress {
        ratio: Some(1.0),
        detail: String::new(),
    }));
    Ok(Outcome::Download(summary))
}

/// Экспортирует cookie выбранного браузера в `cookies.txt`. Вызывается при запуске
/// программы и после смены браузера в настройках, а не перед каждой загрузкой.
pub fn update_cookies(
    ctx: &Ctx,
    browser: Browser,
    yt_dlp: &Path,
    path_env: &OsString,
) -> Result<usize, String> {
    let spec = browser.cookies_spec().ok_or("cookie не используются")?;
    refresh_cookies(
        ctx,
        browser,
        yt_dlp,
        path_env,
        &spec,
        &config::cookies_path(),
    )
}

/// Экспортирует cookie браузера в Netscape-файл, который потом передаётся в `--cookies`.
///
/// yt-dlp сохраняет cookie-jar в файл `--cookies` при завершении, даже если URL
/// не указан (тогда он выходит с кодом 2 «нужен URL»), поэтому сеть тут не нужна.
/// Пишем во временный файл и подменяем основной только при успехе.
fn refresh_cookies(
    ctx: &Ctx,
    browser: Browser,
    yt_dlp: &Path,
    path_env: &OsString,
    spec: &str,
    target: &Path,
) -> Result<usize, String> {
    let dir = target.parent().ok_or("некорректный путь к cookie")?;
    fs::create_dir_all(dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    let tmp = target.with_extension("txt.new");
    let _ = fs::remove_file(&tmp);

    let mut cmd = Command::new(yt_dlp);
    cmd.arg("--ignore-config")
        .args(["--cookies-from-browser", spec])
        .arg("--cookies")
        .arg(&tmp)
        .env("PATH", path_env);

    let mut error = None;
    worker::run(ctx, cmd, |line, _| {
        if let Some(msg) = line.strip_prefix("ERROR: ") {
            error.get_or_insert_with(|| msg.to_string());
        }
    })?;

    let text =
        fs::read_to_string(&tmp).map_err(|_| explain_cookie_error(browser, error.as_deref()))?;
    let count = cookie_lines(&text).count();
    if count == 0 {
        let _ = fs::remove_file(&tmp);
        return Err(explain_cookie_error(browser, error.as_deref()));
    }
    // Экспорт может «пройти», но без сессии YouTube: пользователь вышел из аккаунта или
    // yt-dlp не смог расшифровать часть cookie. Такой файл хуже сохранённого
    // с прошлого запуска, поэтому прошлый оставляем, если в нём вход есть.
    if !has_youtube_login(&text) {
        let previous_ok = fs::read_to_string(target).is_ok_and(|old| has_youtube_login(&old));
        if previous_ok {
            let _ = fs::remove_file(&tmp);
            let reason = error.map_or_else(String::new, |e| format!(": {e}"));
            return Err(format!("в cookie браузера нет входа в YouTube{reason}"));
        }
        ctx.log("В cookie браузера нет входа в YouTube — загрузка будет как без аккаунта");
    }
    fs::rename(&tmp, target).map_err(|e| format!("не удалось сохранить cookies.txt: {e}"))?;
    Ok(count)
}

/// Строки с cookie в Netscape-файле. "#HttpOnly_..." — тоже cookie, а не комментарии.
fn cookie_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && (!l.starts_with('#') || l.starts_with("#HttpOnly_")))
}

/// Есть ли в файле cookie сессии YouTube (пользователь вошёл в аккаунт).
fn has_youtube_login(text: &str) -> bool {
    const SESSION: [&str; 4] = ["SID", "__Secure-1PSID", "__Secure-3PSID", "LOGIN_INFO"];
    cookie_lines(text).any(|line| {
        let fields: Vec<&str> = line.split('\t').collect();
        let [domain, _, _, _, _, name, ..] = fields[..] else {
            return false;
        };
        let domain = domain
            .trim_start_matches("#HttpOnly_")
            .trim_start_matches('.');
        (domain == "youtube.com" || domain.ends_with(".youtube.com")) && SESSION.contains(&name)
    })
}

fn explain_cookie_error(browser: Browser, error: Option<&str>) -> String {
    let raw = error.unwrap_or("в профиле браузера не найдено cookie");
    let lower = raw.to_lowercase();
    // Яндекс Браузер читается движком Chromium (`chrome:<профиль>`), поэтому yt-dlp
    // пишет в ошибках "Chrome". Подменяем, чтобы не путать пользователя.
    let base = match browser {
        Browser::Yandex => raw.replace("Chrome", "Яндекс Браузер"),
        _ => raw.to_string(),
    };
    let hint = if browser == Browser::Safari
        && (lower.contains("permission") || lower.contains("not permitted"))
    {
        " (дайте Терминалу «Полный доступ к диску»: Системные настройки → \
         Конфиденциальность и безопасность, затем перезапустите Терминал)"
    } else if cfg!(target_os = "macos") && (lower.contains("keyring") || lower.contains("keychain"))
    {
        " (разрешите доступ к связке ключей в окне macOS, лучше кнопкой «Разрешать всегда»)"
    } else if cfg!(target_os = "macos") && browser == Browser::Yandex && lower.contains("decrypt") {
        " (на macOS yt-dlp ищет ключ шифрования под именем Chrome, а Яндекс Браузер \
         хранит его под своим, поэтому cookie не расшифровываются; выберите другой браузер)"
    } else if lower.contains("copy") || lower.contains("permission") || lower.contains("locked") {
        match browser {
            Browser::Yandex if cfg!(windows) => {
                " (закройте Яндекс Браузер полностью, включая значок в трее: \
                 в фоновом режиме он продолжает блокировать файл cookie)"
            }
            _ => " (закройте браузер: он блокирует файл cookie)",
        }
    } else if lower.contains("decrypt") || lower.contains("dpapi") {
        " (браузер шифрует cookie способом, который yt-dlp не поддерживает)"
    } else if lower.contains("could not find") {
        match browser {
            Browser::Yandex => " (проверьте, что Яндекс Браузер установлен и есть профиль Default)",
            _ => " (браузер или профиль не найден)",
        }
    } else {
        ""
    };
    format!("{base}{hint}")
}

fn download_command(req: &Request, playlist: bool, cookies: Option<&Path>) -> Command {
    let mut cmd = Command::new(&req.yt_dlp);
    cmd.env("PATH", &req.path_env)
        .arg("--ignore-config")
        .args(["--newline", "--color", "never", "--progress-delta", "0.25"])
        .args([
            "--progress-template",
            "download:[rt] %(progress.status)s %(progress.downloaded_bytes)s \
             %(progress.total_bytes)s %(progress.total_bytes_estimate)s \
             %(progress.speed)s %(progress.eta)s \
             %(progress.fragment_index)s %(progress.fragment_count)s",
        ])
        .arg("--ffmpeg-location")
        .arg(&req.ffmpeg_dir)
        .arg("-P")
        .arg(target_dir(req));

    match req.kind {
        Kind::Video => {
            // Лучшее видео + лучший звук, если раздельных потоков нет — лучший общий.
            cmd.args(["-f", "bv*+ba/b"]);
            if let Some(h) = req.config.quality.max_height() {
                // Сортировка, а не жёсткий фильтр: если нужной высоты нет, возьмётся ближайшая меньшая.
                cmd.args(["-S", &format!("res:{h}")]);
            }
        }
        Kind::Audio => audio_args(&mut cmd, req.config.audio_format, playlist),
    }
    if let Some(path) = cookies {
        cmd.arg("--cookies").arg(path);
    }
    if let Some(proxy) = req.config.proxy_url() {
        cmd.args(["--proxy", &proxy]);
    }
    let template = match (req.kind, playlist) {
        (Kind::Video, true) => {
            "%(playlist_title,playlist_id)s/%(playlist_index)03d - %(title)s.%(ext)s"
        }
        (Kind::Video, false) => "%(title)s.%(ext)s",
        (Kind::Audio, true) => {
            concat!(
                "%(playlist_title,playlist_id)s/%(playlist_index)02d - ",
                "%(artists.0,artist,uploader)s - %(track,title)s.%(ext)s",
            )
        }
        (Kind::Audio, false) => "%(artists.0,artist,uploader)s - %(track,title)s.%(ext)s",
    };
    if playlist {
        cmd.args(["--yes-playlist", "--ignore-errors"]);
    } else {
        cmd.arg("--no-playlist");
    }
    cmd.args(["-o", template]);
    cmd.arg("--").arg(&req.url);
    cmd
}

/// Папка загрузки для вида загрузки.
pub fn target_dir(req: &Request) -> &Path {
    match req.kind {
        Kind::Video => &req.config.download_dir,
        Kind::Audio => &req.config.music_dir,
    }
}

/// Звук в лучшем качестве с квадратной обложкой и тегами.
fn audio_args(cmd: &mut Command, format: AudioFormat, playlist: bool) {
    match format {
        // -x без --audio-format перекладывает opus из webm в .opus без перекодирования.
        AudioFormat::Opus => cmd.args(["-f", "ba[acodec=opus]/ba", "-x"]),
        AudioFormat::M4a => cmd.args(["-f", "ba[ext=m4a]/ba", "-x", "--audio-format", "m4a"]),
        AudioFormat::Mp3 => cmd.args([
            "-f",
            "ba",
            "-x",
            "--audio-format",
            "mp3",
            "--audio-quality",
            "0",
        ]),
    };
    cmd.args([
        "--embed-thumbnail",
        "--embed-metadata",
        "--convert-thumbnails",
        "jpg",
    ])
    .args(["--ppa", SQUARE_COVER])
    // Дата релиза вместо даты загрузки на YouTube.
    .args([
        "--parse-metadata",
        "%(release_date,release_year,upload_date)s:%(meta_date)s",
    ])
    .args([
        "--parse-metadata",
        "%(album_artist,artists.0,artist)s:%(meta_album_artist)s",
    ])
    // YouTube пишет в жанр «Music», а в описание — служебный текст лейбла.
    .args(["--parse-metadata", ":(?P<meta_genre>)"])
    .args(["--parse-metadata", ":(?P<meta_description>)"])
    .args(["--parse-metadata", ":(?P<meta_synopsis>)"]);
    if playlist {
        cmd.args(["--parse-metadata", "%(playlist_index)s:%(meta_track)s"])
            // Альбом YT Music называется «Album - <название>».
            .args(["--replace-in-metadata", "playlist_title", "^Album - ", ""]);
    }
}

/// Превращает вывод yt-dlp в шаги и прогресс.
struct OutputParser<'a> {
    ctx: &'a Ctx,
    kind: Kind,
    /// Сколько потоков у текущего видео (2 — видео + аудио) и какой сейчас качается.
    parts: usize,
    part: usize,
    downloading: bool,
    summary: DownloadSummary,
}

impl<'a> OutputParser<'a> {
    fn new(ctx: &'a Ctx, kind: Kind) -> Self {
        Self {
            ctx,
            kind,
            parts: 1,
            part: 0,
            downloading: false,
            summary: DownloadSummary::default(),
        }
    }

    fn finish(self) -> DownloadSummary {
        self.summary
    }

    fn feed(&mut self, line: &str, is_err: bool) {
        if let Some(rest) = line.strip_prefix(PROGRESS_TAG) {
            self.ctx.send(Event::Progress(parse_progress(rest)));
            return;
        }
        if let Some(rest) = line.strip_prefix("[download] ") {
            self.on_download_line(rest);
            return;
        }
        if let Some(rest) = line.strip_prefix("[info] ") {
            if let Some((_, formats)) = rest.split_once("format(s): ") {
                self.parts = formats.split('+').count();
                self.part = 0;
                self.downloading = false;
                self.ctx.step("Подготовка к загрузке");
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("[Merger] ") {
            self.ctx.step("Объединение видео и аудио (ffmpeg)");
            if let Some(path) = quoted(rest) {
                self.summary.files.push(path);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("[ExtractAudio] ") {
            self.ctx.step("Извлечение звука (ffmpeg)");
            // Файл после извлечения звука заменяет скачанный webm/m4a.
            if let Some(path) = rest.strip_prefix("Destination: ") {
                self.summary.files.pop();
                self.summary.files.push(path.to_string());
            }
            return;
        }
        if line.starts_with("[ThumbnailsConvertor]") {
            self.ctx.step("Подготовка обложки");
            return;
        }
        if line.starts_with("[EmbedThumbnail]") {
            self.ctx.step("Встраивание обложки");
            return;
        }
        if line.starts_with("[Metadata]") {
            self.ctx.step("Запись тегов");
            return;
        }
        if line.starts_with("[Fixup") || line.starts_with("[VideoConvertor]") {
            self.ctx.step("Обработка файла");
            return;
        }
        if let Some(msg) = line.strip_prefix("ERROR: ") {
            self.summary.errors.push(msg.to_string());
            self.ctx.log(format!("Ошибка: {msg}"));
            return;
        }
        if let Some(msg) = line.strip_prefix("WARNING: ") {
            // Альбомы YT Music yt-dlp штатно открывает как плейлист youtube.com — это не проблема.
            if msg.contains("YouTube Music is not directly supported") {
                return;
            }
            self.ctx.log(format!("Предупреждение: {msg}"));
            return;
        }
        if line.starts_with('[') && !self.downloading {
            // [youtube] / [youtube:tab] — извлечение информации.
            self.ctx.step(match self.kind {
                Kind::Video => "Получение информации о видео",
                Kind::Audio => "Получение информации о треке",
            });
            return;
        }
        // Строки с отступом — продолжение многострочных предупреждений yt-dlp.
        if is_err && !line.starts_with(' ') {
            self.ctx.log(line);
        }
    }

    fn on_download_line(&mut self, rest: &str) {
        let item = rest
            .strip_prefix("Downloading item ")
            .or_else(|| rest.strip_prefix("Downloading video "));
        if let Some(item) = item {
            if let Some((i, n)) = item.split_once(" of ")
                && let (Ok(index), Ok(total)) = (i.trim().parse(), n.trim().parse())
            {
                self.ctx.send(Event::Item { index, total });
            }
            self.downloading = false;
            return;
        }
        if let Some(path) = rest.strip_prefix("Destination: ") {
            self.downloading = true;
            self.part += 1;
            let what = match (self.parts, self.part) {
                (2, 1) => "Загрузка видео",
                (2, 2) => "Загрузка аудио",
                _ if self.kind == Kind::Audio => "Загрузка звука",
                (n, p) if n > 2 => {
                    return self
                        .ctx
                        .step(format!("Загрузка потока {p} из {n}: {}", file_name(path)));
                }
                _ => "Загрузка",
            };
            self.ctx.step(format!("{what}: {}", file_name(path)));
            self.ctx.send(Event::Progress(Progress::default()));
            if self.parts <= 1 {
                self.summary.files.push(path.to_string());
            }
            return;
        }
        if let Some(path) = rest.strip_suffix(" has already been downloaded") {
            self.ctx.step(format!("Уже скачано: {}", file_name(path)));
            self.summary.files.push(path.to_string());
            return;
        }
        if let Some(name) = rest.strip_prefix("Downloading playlist: ") {
            self.ctx.log(format!("Плейлист: {name}"));
        }
    }
}

fn parse_progress(rest: &str) -> Progress {
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let num = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok());
    let (downloaded, total, estimate, speed, eta) = (num(1), num(2), num(3), num(4), num(5));
    let (frag_index, frag_count) = (num(6), num(7));
    let total = total.or(estimate);

    let ratio = match (downloaded, total, frag_index, frag_count) {
        (Some(d), Some(t), _, _) if t > 0.0 => Some(d / t),
        (_, _, Some(i), Some(n)) if n > 0.0 => Some(i / n),
        _ => None,
    }
    .map(|r| r.clamp(0.0, 1.0));

    let mut parts = Vec::new();
    match (downloaded, total) {
        (Some(d), Some(t)) => parts.push(format!("{} из {}", human_bytes(d), human_bytes(t))),
        (Some(d), None) => parts.push(human_bytes(d)),
        _ => {}
    }
    if let Some(s) = speed {
        parts.push(format!("{}/с", human_bytes(s)));
    }
    if let Some(e) = eta {
        parts.push(format!("осталось {}", human_duration(e as u64)));
    }
    Progress {
        ratio,
        detail: parts.join(" · "),
    }
}

fn human_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let end = text.rfind('"')?;
    (end > start).then(|| text[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_youtube_login() {
        let logged_in = "# Netscape HTTP Cookie File\n\
            .google.com\tTRUE\t/\tTRUE\t0\tNID\tx\n\
            #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-3PSID\tx\n";
        assert!(has_youtube_login(logged_in));

        let anonymous = "# Netscape HTTP Cookie File\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\tVISITOR_INFO1_LIVE\tx\n\
            .google.com\tTRUE\t/\tTRUE\t0\tSID\tx\n";
        assert!(!has_youtube_login(anonymous));
        assert_eq!(cookie_lines(anonymous).count(), 2);
    }

    #[test]
    fn detects_playlists() {
        assert!(is_playlist("https://www.youtube.com/playlist?list=PL123"));
        assert!(is_playlist(
            "https://www.youtube.com/watch?v=abc&list=PL123"
        ));
        assert!(!is_playlist("https://youtu.be/abc"));
    }

    #[test]
    fn detects_video_in_playlist() {
        assert!(is_video_in_playlist(
            "https://www.youtube.com/watch?v=abc&list=PL123"
        ));
        assert!(is_video_in_playlist(
            "https://www.youtube.com/watch?list=PL123&v=abc"
        ));
        assert!(is_video_in_playlist("https://youtu.be/abc?list=PL123"));
        assert!(!is_video_in_playlist(
            "https://www.youtube.com/playlist?list=PL123"
        ));
        assert!(!is_video_in_playlist("https://www.youtube.com/watch?v=abc"));
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url(" youtu.be/abc ").as_deref(),
            Some("https://youtu.be/abc")
        );
        assert_eq!(
            normalize_url("https://x.y/z").as_deref(),
            Some("https://x.y/z")
        );
        assert_eq!(normalize_url("hello world"), None);
        assert_eq!(normalize_url("hello"), None);
    }

    #[test]
    fn parses_progress() {
        let p = parse_progress(" downloading 512 1024 NA 2048.5 3 NA NA");
        assert_eq!(p.ratio, Some(0.5));
        assert_eq!(p.detail, "512 Б из 1.0 КБ · 2.0 КБ/с · осталось 0:03");

        let p = parse_progress(" downloading 100 NA NA NA NA 5 10");
        assert_eq!(p.ratio, Some(0.5));

        let p = parse_progress(" downloading NA NA NA NA NA NA NA");
        assert_eq!(p.ratio, None);
    }

    #[test]
    fn builds_audio_command() {
        let req = Request {
            url: "https://music.youtube.com/browse/MPREb_x".into(),
            kind: Kind::Audio,
            playlist: true,
            config: Config::default(),
            yt_dlp: "yt-dlp".into(),
            ffmpeg_dir: "ffmpeg".into(),
            path_env: OsString::new(),
        };
        let cmd = download_command(&req, true, None);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let after = |flag: &str| {
            let i = args.iter().position(|a| a == flag).unwrap();
            args[i + 1].clone()
        };
        assert_eq!(after("-f"), "ba[acodec=opus]/ba");
        assert_eq!(after("-P"), config::default_music_dir().to_string_lossy());
        assert_eq!(
            after("-o"),
            "%(playlist_title,playlist_id)s/%(playlist_index)02d - %(artists.0,artist,uploader)s - %(track,title)s.%(ext)s"
        );
        assert_eq!(
            after("--ppa"),
            r#"ThumbnailsConvertor+ffmpeg_o:-c:v mjpeg -qmin 1 -qscale:v 1 -vf "crop='min(iw,ih)':'min(iw,ih)'""#
        );
        assert!(args.contains(&"--embed-thumbnail".to_string()));
        assert!(args.contains(&"%(playlist_index)s:%(meta_track)s".to_string()));
        assert!(!args.iter().any(|a| a == "bv*+ba/b"));
    }

    #[test]
    fn extracts_quoted_path() {
        assert_eq!(
            quoted(r#"Merging formats into "C:\Videos\a b.mkv""#).as_deref(),
            Some(r"C:\Videos\a b.mkv")
        );
    }
}
