//! Состояние приложения, сценарий запуска и обработка ввода.

use std::{
    collections::VecDeque, fs, path::PathBuf, process::Command, sync::mpsc::TryRecvError,
    time::Duration,
};

use anyhow::Result;
use ratatui::{
    DefaultTerminal,
    crossterm::event::{
        self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    },
    layout::{Position, Rect},
};

use crate::{
    config::{self, AudioFormat, Browser, Config, Quality, Theme},
    download::{self, DownloadSummary, Kind},
    input::{Input, clipboard_text},
    music::{self, Track},
    packages::{self, AppUpdate, Packages},
    ui,
    worker::{CANCELLED, Ctx, Event, Outcome, Progress, Worker},
};

const LOG_LIMIT: usize = 200;

pub enum Stage {
    /// Проверки и установка зависимостей.
    Startup,
    Ready,
    Fatal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    InstallManager,
    InstallApps,
    CheckUpdates,
    UpdateApps,
    RefreshCookies,
    Download,
    Search,
}

/// Режим главного экрана: загрузка видео по ссылке или поиск музыки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Video,
    Music,
}

#[derive(Default)]
pub struct StartupView {
    pub title: String,
    pub step: String,
    pub log: VecDeque<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    /// Список результатов поиска музыки.
    Results,
    Settings,
    Exit,
}

enum Action {
    InstallApps(Vec<String>),
    UpdateApps(Vec<String>),
    Continue,
    Quit,
    CancelDownload,
    CancelAndQuit,
    /// Начать загрузку: ссылка, «весь плейлист или только видео» и что качать.
    Download(String, bool, Kind),
    Nothing,
}

pub struct Confirm {
    pub title: String,
    pub body: Vec<String>,
    pub yes: &'static str,
    pub no: &'static str,
    pub yes_selected: bool,
    on_yes: Action,
    on_no: Action,
    /// Esc закрывает окно без действия. Иначе Esc — это «нет».
    esc_closes: bool,
}

pub mod field {
    pub const QUALITY: usize = 0;
    pub const AUDIO: usize = 1;
    pub const THEME: usize = 2;
    pub const BROWSER: usize = 3;
    pub const PROXY: usize = 4;
    pub const FOLDER: usize = 5;
    pub const BROWSE: usize = 6;
    pub const MUSIC_FOLDER: usize = 7;
    pub const MUSIC_BROWSE: usize = 8;
    pub const SAVE: usize = 9;
    pub const CANCEL: usize = 10;
}

pub struct SettingsForm {
    pub quality: usize,
    pub audio: usize,
    /// Пока окно открыто, интерфейс рисуется выбранной здесь темой (предпросмотр).
    pub theme: usize,
    pub browser: usize,
    /// Порт SOCKS5-прокси; пусто — без прокси.
    pub proxy: Input,
    pub folder: Input,
    pub music_folder: Input,
    pub focus: usize,
    /// Первичная настройка: отменить нельзя, только сохранить.
    pub first_run: bool,
    pub error: Option<String>,
}

impl SettingsForm {
    fn fields(&self) -> usize {
        if self.first_run {
            field::SAVE + 1
        } else {
            field::CANCEL + 1
        }
    }

    fn is_text_field(&self) -> bool {
        matches!(
            self.focus,
            field::PROXY | field::FOLDER | field::MUSIC_FOLDER
        )
    }

    fn is_selector(&self) -> bool {
        matches!(
            self.focus,
            field::QUALITY | field::AUDIO | field::THEME | field::BROWSER
        )
    }

    /// Вставка текста в поле под фокусом; в поле порта попадают только цифры.
    fn paste(&mut self, text: &str) {
        match self.focus {
            field::PROXY => {
                let digits: String = text.chars().filter(char::is_ascii_digit).collect();
                self.proxy.insert_str(&digits);
            }
            field::FOLDER => self.folder.insert_str(text.trim()),
            field::MUSIC_FOLDER => self.music_folder.insert_str(text.trim()),
            _ => {}
        }
    }
}

pub enum Modal {
    Confirm(Confirm),
    Settings(SettingsForm),
}

pub struct StepEntry {
    pub text: String,
    pub done: bool,
}

pub struct DownloadView {
    /// Что показывать в шапке панели: ссылка или «Исполнитель — Трек».
    pub label: String,
    pub kind: Kind,
    /// Папка загрузки (для «Открыть папку»).
    pub dir: PathBuf,
    pub playlist: bool,
    pub steps: Vec<StepEntry>,
    pub progress: Progress,
    pub item: Option<(usize, usize)>,
    pub log: VecDeque<String>,
    pub result: Option<Result<DownloadSummary, String>>,
}

impl DownloadView {
    fn set_step(&mut self, text: String) {
        if self.steps.last().is_some_and(|s| s.text == text) {
            return;
        }
        if let Some(last) = self.steps.last_mut() {
            last.done = true;
        }
        self.steps.push(StepEntry { text, done: false });
        if self.steps.len() > 50 {
            self.steps.remove(0);
        }
    }

    pub fn running(&self) -> bool {
        self.result.is_none()
    }
}

/// Результаты поиска в YouTube Music.
pub struct SearchView {
    pub query: String,
    pub results: Vec<Track>,
    pub selected: usize,
    pub running: bool,
    pub error: Option<String>,
}

impl SearchView {
    fn selected_track(&self) -> Option<&Track> {
        self.results.get(self.selected)
    }

    fn move_selection(&mut self, delta: isize) {
        let last = self.results.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }
}

/// Области, по которым можно кликнуть мышью (заполняются при отрисовке).
#[derive(Default)]
pub struct Hits {
    pub settings: Rect,
    pub exit: Rect,
    pub input: Rect,
    pub open_folder: Rect,
    /// Вкладки режимов «Видео» и «Музыка».
    pub modes: Vec<(Rect, Mode)>,
    /// Область списка результатов и строки с индексами треков.
    pub results: Rect,
    pub result_rows: Vec<(Rect, usize)>,
    /// Для модальных окон: индекс поля настроек или 0 = «Да», 1 = «Нет».
    pub modal: Vec<(Rect, usize)>,
}

pub struct App {
    pub packages: Packages,
    pub config: Config,
    pub first_run: bool,
    pub stage: Stage,
    pub startup: StartupView,
    pub modal: Option<Modal>,
    pub mode: Mode,
    /// Поле ссылки (режим видео).
    pub input: Input,
    /// Поле поиска (режим музыки).
    pub query: Input,
    pub focus: Focus,
    pub download: Option<DownloadView>,
    pub search: Option<SearchView>,
    pub notice: Option<String>,
    pub hits: Hits,
    pub tick: usize,
    worker: Option<(Job, Worker)>,
    updates_checked: bool,
    /// Cookie уже экспортированы из браузера в этом запуске (или это не требуется).
    cookies_refreshed: bool,
    quit: bool,
}

impl App {
    pub fn new() -> App {
        let loaded = Config::load();
        App {
            packages: Packages::locate(),
            first_run: loaded.is_none(),
            config: loaded.unwrap_or_default(),
            stage: Stage::Startup,
            startup: StartupView::default(),
            modal: None,
            mode: Mode::Video,
            input: Input::default(),
            query: Input::default(),
            focus: Focus::Input,
            download: None,
            search: None,
            notice: None,
            hits: Hits::default(),
            tick: 0,
            worker: None,
            updates_checked: false,
            cookies_refreshed: false,
            quit: false,
        }
    }

    /// Возвращает текст фатальной ошибки, если программа закрылась из-за неё:
    /// после выхода из TUI его печатают в терминал, чтобы команды из него
    /// (например, установку Homebrew) можно было скопировать.
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<Option<String>> {
        self.advance_startup();
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, &mut self))?;
            if event::poll(Duration::from_millis(100))? {
                // Вставленный текст приходит пачкой событий — обрабатываем всё до перерисовки.
                loop {
                    self.on_event(event::read()?);
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            self.pump_worker();
            self.tick = self.tick.wrapping_add(1);
        }
        if let Some((_, worker)) = self.worker.take() {
            worker.cancel();
        }
        Ok(match self.stage {
            Stage::Fatal(message) => Some(message),
            _ => None,
        })
    }

    pub fn busy(&self) -> bool {
        self.worker.is_some()
    }

    pub fn downloading(&self) -> bool {
        matches!(self.worker, Some((Job::Download, _)))
    }

    pub fn searching(&self) -> bool {
        matches!(self.worker, Some((Job::Search, _)))
    }

    /// Поле ввода текущего режима.
    pub fn current_input(&self) -> &Input {
        match self.mode {
            Mode::Video => &self.input,
            Mode::Music => &self.query,
        }
    }

    fn current_input_mut(&mut self) -> &mut Input {
        match self.mode {
            Mode::Video => &mut self.input,
            Mode::Music => &mut self.query,
        }
    }

    pub fn has_results(&self) -> bool {
        self.mode == Mode::Music && self.search.as_ref().is_some_and(|s| !s.results.is_empty())
    }

    // ---------------------------------------------------------------- запуск

    /// Сценарий подготовки: менеджер пакетов (scoop/Homebrew) → yt-dlp/ffmpeg → обновления
    /// → первичная настройка → cookie. Вызывается заново после завершения каждого шага.
    fn advance_startup(&mut self) {
        self.stage = Stage::Startup;
        if !self.packages.is_installed() {
            match self.packages.install_manager() {
                Ok(cmd) => {
                    let title = format!("Установка {}", packages::MANAGER);
                    self.start_job(Job::InstallManager, &title, move |ctx| {
                        ctx.step(format!("{} не найден — устанавливаю", packages::MANAGER));
                        let status = crate::worker::run(ctx, cmd, |line, _| ctx.log(line))?;
                        if status.success() {
                            Ok(Outcome::Done)
                        } else {
                            Err(format!(
                                "установщик {} завершился с ошибкой",
                                packages::MANAGER
                            ))
                        }
                    });
                }
                Err(message) => self.fatal(message),
            }
            return;
        }

        let missing = self.packages.missing_apps();
        if !missing.is_empty() {
            let mut body = vec![
                format!(
                    "Для работы программы нужны компоненты, которых нет в {}:",
                    packages::MANAGER
                ),
                String::new(),
            ];
            body.extend(
                missing
                    .iter()
                    .map(|app| format!("  • {app} — {}", describe_app(app))),
            );
            body.push(String::new());
            body.push("Установить их сейчас? Без них программа будет закрыта.".into());
            self.modal = Some(Modal::Confirm(Confirm {
                title: "Необходимые компоненты".into(),
                body,
                yes: "Установить",
                no: "Выйти",
                yes_selected: true,
                on_yes: Action::InstallApps(missing),
                on_no: Action::Quit,
                esc_closes: false,
            }));
            return;
        }

        if !self.updates_checked {
            let packages = self.packages.clone();
            self.start_job(
                Job::CheckUpdates,
                "Проверка обновлений",
                move |ctx| packages.check_updates(ctx).map(Outcome::Updates),
            );
            return;
        }

        if self.first_run {
            self.open_settings(true);
            return;
        }

        if !self.cookies_refreshed && self.config.browser != Browser::None {
            let browser = self.config.browser;
            let yt_dlp = self.packages.yt_dlp();
            let path_env = self.packages.path_env();
            self.start_job(
                Job::RefreshCookies,
                "Получение cookie",
                move |ctx| {
                    ctx.step(format!(
                        "Получение cookie: {} (Esc — пропустить)",
                        browser.label()
                    ));
                    download::update_cookies(ctx, browser, &yt_dlp, &path_env).map(|count| {
                        ctx.log(format!("Получено cookie из браузера: {count}"));
                        Outcome::Done
                    })
                },
            );
            return;
        }

        self.stage = Stage::Ready;
    }

    fn install_apps(&mut self, apps: Vec<String>, job: Job) {
        let (title, cmd) = match job {
            Job::UpdateApps => (
                "Обновление компонентов",
                self.packages.upgrade_command(&apps),
            ),
            _ => (
                "Установка компонентов",
                self.packages.install_command(&apps),
            ),
        };
        self.start_job(job, title, move |ctx| {
            ctx.step(format!("{title} ({}): {}", packages::CLI, apps.join(" ")));
            let status = crate::worker::run(ctx, cmd, |line, _| ctx.log(line))?;
            if status.success() {
                Ok(Outcome::Done)
            } else {
                Err(format!("{} завершился с ошибкой", packages::CLI))
            }
        });
    }

    fn start_job<F>(&mut self, job: Job, title: &str, body: F)
    where
        F: FnOnce(&Ctx) -> Result<Outcome, String> + Send + 'static,
    {
        if !matches!(job, Job::Download | Job::Search) {
            self.stage = Stage::Startup;
            self.startup.title = title.to_string();
            self.startup.step = title.to_string();
            self.startup.log.clear();
        }
        self.worker = Some((job, Worker::spawn(body)));
    }

    fn pump_worker(&mut self) {
        let Some((job, worker)) = &self.worker else {
            return;
        };
        let job = *job;
        let mut events: Vec<Event> = Vec::new();
        let mut finished = None;
        loop {
            match worker.rx.try_recv() {
                Ok(Event::Finished(result)) => {
                    finished = Some(result);
                    break;
                }
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = Some(Err("фоновая задача аварийно завершилась".into()));
                    break;
                }
            }
        }
        for event in events {
            self.apply_event(job, event);
        }
        if let Some(result) = finished {
            self.worker = None;
            self.on_finished(job, result);
        }
    }

    fn apply_event(&mut self, job: Job, event: Event) {
        match job {
            Job::Download => {
                let Some(view) = self.download.as_mut() else {
                    return;
                };
                match event {
                    Event::Log(line) => push_log(&mut view.log, line),
                    Event::Step(text) => view.set_step(text),
                    Event::Progress(progress) => view.progress = progress,
                    Event::Item { index, total } => {
                        view.item = Some((index, total));
                        let what = match view.kind {
                            Kind::Video => "Видео",
                            Kind::Audio => "Трек",
                        };
                        view.set_step(format!("{what} {index} из {total}"));
                    }
                    Event::Track(_) | Event::Finished(_) => {}
                }
            }
            Job::Search => {
                if let (Event::Track(track), Some(search)) = (event, self.search.as_mut())
                    && !search.results.iter().any(|t| t.id == track.id)
                {
                    search.results.push(track);
                }
            }
            _ => match event {
                Event::Log(line) => push_log(&mut self.startup.log, line),
                Event::Step(text) => self.startup.step = text,
                _ => {}
            },
        }
    }

    fn on_finished(&mut self, job: Job, result: Result<Outcome, String>) {
        match job {
            Job::InstallManager => match result {
                Ok(_) if self.packages.is_installed() => self.advance_startup(),
                Ok(_) => self.fatal(format!(
                    "Установщик отработал, но {} не найден в {}.",
                    packages::MANAGER,
                    self.packages.location()
                )),
                Err(e) => self.fatal(format!("Не удалось установить {}: {e}", packages::MANAGER)),
            },
            Job::InstallApps => {
                let missing = self.packages.missing_apps();
                if missing.is_empty() {
                    // Только что поставили свежие версии — проверять обновления незачем.
                    self.updates_checked = true;
                    self.advance_startup();
                } else {
                    let reason = result.err().unwrap_or_default();
                    self.fatal(format!(
                        "Не удалось установить: {}. {reason}\nПопробуйте вручную: {} install {}",
                        missing.join(", "),
                        packages::CLI,
                        missing.join(" ")
                    ));
                }
            }
            Job::CheckUpdates => {
                self.updates_checked = true;
                match result {
                    Ok(Outcome::Updates(updates)) if !updates.is_empty() => {
                        self.ask_updates(updates)
                    }
                    _ => self.advance_startup(),
                }
            }
            Job::UpdateApps => {
                if let Err(e) = result {
                    self.notice = Some(format!("Обновление не выполнено: {e}"));
                }
                self.advance_startup();
            }
            Job::RefreshCookies => {
                self.cookies_refreshed = true;
                if let Err(e) = result {
                    let fallback = if config::cookies_path().is_file() {
                        "используются ранее сохранённые"
                    } else {
                        "загрузка будет без cookie"
                    };
                    self.notice = Some(format!("Cookie не обновлены ({fallback}): {e}"));
                }
                self.advance_startup();
            }
            Job::Download => {
                if let Some(view) = self.download.as_mut() {
                    if let Some(last) = view.steps.last_mut() {
                        last.done = result.is_ok();
                    }
                    view.result = Some(match result {
                        Ok(Outcome::Download(summary)) => Ok(summary),
                        Ok(_) => Ok(DownloadSummary::default()),
                        Err(e) => Err(e),
                    });
                }
                // Браузер сменили в настройках во время загрузки — обновляем cookie сейчас.
                if !self.cookies_refreshed {
                    self.advance_startup();
                }
            }
            Job::Search => {
                if let Some(search) = self.search.as_mut() {
                    search.running = false;
                    match result {
                        Err(e) if e != CANCELLED => search.error = Some(e),
                        _ if search.results.is_empty() => {
                            search.error = Some("Ничего не найдено".into())
                        }
                        _ => {}
                    }
                }
                if !self.cookies_refreshed {
                    self.advance_startup();
                }
            }
        }
    }

    fn ask_updates(&mut self, updates: Vec<AppUpdate>) {
        let mut body = vec!["Доступны новые версии:".to_string(), String::new()];
        body.extend(
            updates
                .iter()
                .map(|u| format!("  • {}: {} → {}", u.name, u.installed, u.latest)),
        );
        body.push(String::new());
        body.push("Обновить сейчас?".into());
        self.modal = Some(Modal::Confirm(Confirm {
            title: "Обновления".into(),
            body,
            yes: "Обновить",
            no: "Позже",
            yes_selected: true,
            on_yes: Action::UpdateApps(updates.into_iter().map(|u| u.name).collect()),
            on_no: Action::Continue,
            esc_closes: false,
        }));
    }

    fn fatal(&mut self, message: String) {
        self.stage = Stage::Fatal(message);
    }

    // ---------------------------------------------------------------- действия

    fn run_action(&mut self, action: Action) {
        match action {
            Action::InstallApps(apps) => self.install_apps(apps, Job::InstallApps),
            Action::UpdateApps(apps) => self.install_apps(apps, Job::UpdateApps),
            Action::Continue => self.advance_startup(),
            Action::Quit => self.quit = true,
            Action::CancelDownload => self.cancel_worker(),
            Action::CancelAndQuit => {
                self.cancel_worker();
                self.quit = true;
            }
            Action::Download(url, playlist, kind) => {
                self.clear_input(kind);
                self.start_download(url, playlist, kind, None);
            }
            Action::Nothing => {}
        }
    }

    fn cancel_worker(&self) {
        if let Some((_, worker)) = &self.worker {
            worker.cancel();
        }
    }

    fn request_quit(&mut self) {
        if self.downloading() {
            self.confirm_simple(
                "Выход",
                "Идёт загрузка. Прервать её и выйти?",
                Action::CancelAndQuit,
            );
        } else {
            self.quit = true;
        }
    }

    fn confirm_simple(&mut self, title: &str, text: &str, on_yes: Action) {
        self.modal = Some(Modal::Confirm(Confirm {
            title: title.into(),
            body: vec![text.into()],
            yes: "Да",
            no: "Нет",
            yes_selected: false,
            on_yes,
            on_no: Action::Nothing,
            esc_closes: false,
        }));
    }

    fn open_settings(&mut self, first_run: bool) {
        self.modal = Some(Modal::Settings(SettingsForm {
            quality: self.config.quality.index(),
            audio: self.config.audio_format.index(),
            theme: self.config.theme.index(),
            browser: self.config.browser.index(),
            proxy: Input::new(
                self.config
                    .proxy_port
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            folder: Input::new(self.config.download_dir.to_string_lossy()),
            music_folder: Input::new(self.config.music_dir.to_string_lossy()),
            focus: field::QUALITY,
            first_run,
            error: None,
        }));
    }

    fn switch_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Video => Mode::Music,
            Mode::Music => Mode::Video,
        };
        self.focus = Focus::Input;
        self.notice = None;
    }

    fn submit(&mut self) {
        match self.mode {
            Mode::Video => self.submit_url(),
            Mode::Music => self.submit_query(),
        }
    }

    fn submit_url(&mut self) {
        let Some(url) = download::normalize_url(self.input.value()) else {
            self.notice = Some("Это не похоже на ссылку YouTube".into());
            return;
        };
        self.begin_download(url, Kind::Video);
    }

    /// Enter в режиме музыки: ссылку качаем сразу, остальное ищем.
    fn submit_query(&mut self) {
        let text = self.query.value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(url) = download::normalize_url(&text) {
            self.begin_download(url, Kind::Audio);
            return;
        }
        if self.downloading() {
            self.notice =
                Some("Поиск станет доступен после окончания загрузки (Esc — прервать)".into());
            return;
        }
        self.stop_search();
        self.notice = None;
        self.search = Some(SearchView {
            query: text.clone(),
            results: Vec::new(),
            selected: 0,
            running: true,
            error: None,
        });
        let request = music::SearchRequest {
            query: text,
            config: self.config.clone(),
            yt_dlp: self.packages.yt_dlp(),
            path_env: self.packages.path_env(),
        };
        self.start_job(Job::Search, "", move |ctx| music::run(ctx, request));
    }

    /// Прерывает идущий поиск, оставляя уже найденное.
    fn stop_search(&mut self) {
        if !self.searching() {
            return;
        }
        if let Some((_, worker)) = self.worker.take() {
            worker.cancel();
        }
        if let Some(search) = self.search.as_mut() {
            search.running = false;
            if search.results.is_empty() {
                search.error = Some("Поиск прерван".into());
            }
        }
    }

    /// Проверки перед загрузкой по ссылке; для видео из плейлиста — вопрос, что качать.
    fn begin_download(&mut self, url: String, kind: Kind) {
        if self.downloading() {
            self.notice = Some("Дождитесь окончания текущей загрузки или прервите её (Esc)".into());
            return;
        }
        if !self.ensure_dir(kind) {
            return;
        }
        self.notice = None;
        if download::is_video_in_playlist(&url) {
            let (what, this) = match kind {
                Kind::Video => ("видео", "только это видео"),
                Kind::Audio => ("трек", "только этот трек"),
            };
            // Ссылка остаётся в поле, пока пользователь не выберет (Esc — отмена).
            self.modal = Some(Modal::Confirm(Confirm {
                title: "Плейлист".into(),
                body: vec![
                    format!("Это {what} из плейлиста."),
                    String::new(),
                    format!("Скачать весь плейлист или {this}?"),
                ],
                yes: "Весь плейлист",
                no: match kind {
                    Kind::Video => "Только это видео",
                    Kind::Audio => "Только этот трек",
                },
                yes_selected: false,
                on_yes: Action::Download(url.clone(), true, kind),
                on_no: Action::Download(url, false, kind),
                esc_closes: true,
            }));
            return;
        }
        let playlist = download::is_playlist(&url) || music::is_album(&url);
        self.clear_input(kind);
        self.start_download(url, playlist, kind, None);
    }

    /// Enter на результате поиска.
    fn download_selected(&mut self) {
        let Some(track) = self
            .search
            .as_ref()
            .and_then(SearchView::selected_track)
            .cloned()
        else {
            return;
        };
        if self.downloading() {
            self.notice = Some("Дождитесь окончания текущей загрузки или прервите её (Esc)".into());
            return;
        }
        if !self.ensure_dir(Kind::Audio) {
            return;
        }
        // Одновременно работает одна задача: поиск останавливаем, найденное остаётся.
        self.stop_search();
        self.notice = None;
        let label = match track.artist() {
            "" => track.name().to_string(),
            artist => format!("{artist} — {}", track.name()),
        };
        self.start_download(track.url(), false, Kind::Audio, Some(label));
    }

    fn dir_for(&self, kind: Kind) -> PathBuf {
        match kind {
            Kind::Video => self.config.download_dir.clone(),
            Kind::Audio => self.config.music_dir.clone(),
        }
    }

    fn ensure_dir(&mut self, kind: Kind) -> bool {
        match fs::create_dir_all(self.dir_for(kind)) {
            Ok(()) => true,
            Err(e) => {
                self.notice = Some(format!("Папка загрузки недоступна: {e}"));
                false
            }
        }
    }

    fn clear_input(&mut self, kind: Kind) {
        match kind {
            Kind::Video => self.input.clear(),
            Kind::Audio => self.query.clear(),
        }
    }

    fn start_download(&mut self, url: String, playlist: bool, kind: Kind, label: Option<String>) {
        self.download = Some(DownloadView {
            label: label.unwrap_or_else(|| url.clone()),
            kind,
            dir: self.dir_for(kind),
            playlist,
            steps: Vec::new(),
            progress: Progress::default(),
            item: None,
            log: VecDeque::new(),
            result: None,
        });
        let request = download::Request {
            url,
            kind,
            playlist,
            config: self.config.clone(),
            yt_dlp: self.packages.yt_dlp(),
            ffmpeg_dir: self.packages.ffmpeg_dir(),
            path_env: self.packages.path_env(),
        };
        self.start_job(Job::Download, "", move |ctx| download::run(ctx, request));
    }

    /// Папка последней загрузки, а если загрузок не было — папка текущего режима.
    fn open_download_folder(&self) {
        let dir = match &self.download {
            Some(view) => view.dir.clone(),
            None => self.dir_for(match self.mode {
                Mode::Video => Kind::Video,
                Mode::Music => Kind::Audio,
            }),
        };
        let opener = if cfg!(windows) {
            "explorer.exe"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let mut cmd = Command::new(opener);
        cmd.arg(dir);
        let _ = cmd.spawn();
    }

    fn paste_into_input(&mut self) {
        if let Some(text) = clipboard_text() {
            self.current_input_mut().insert_str(text.trim());
        }
    }

    // ---------------------------------------------------------------- ввод

    fn on_event(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::Paste(text) => match &mut self.modal {
                Some(Modal::Settings(form)) => form.paste(&text),
                None => {
                    self.focus = Focus::Input;
                    self.current_input_mut().insert_str(text.trim());
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if let Stage::Fatal(_) = self.stage {
            self.quit = true;
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.cancel_worker();
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        if key.code == KeyCode::F(10) {
            self.request_quit();
            return;
        }
        if let Stage::Startup = self.stage {
            if key.code == KeyCode::Esc {
                self.cancel_worker();
            }
            return;
        }

        match key.code {
            KeyCode::F(2) => return self.open_settings(false),
            KeyCode::F(3) => return self.switch_mode(),
            KeyCode::Char('o') if ctrl => return self.open_download_folder(),
            KeyCode::Tab | KeyCode::BackTab => {
                // Список результатов — в цикле, только когда он есть.
                let mut order = vec![Focus::Input];
                if self.has_results() {
                    order.push(Focus::Results);
                }
                order.extend([Focus::Settings, Focus::Exit]);
                let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let n = order.len();
                let next = if key.code == KeyCode::Tab {
                    i + 1
                } else {
                    i + n - 1
                };
                self.focus = order[next % n];
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Settings | Focus::Exit => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if self.focus == Focus::Settings {
                        self.open_settings(false);
                    } else {
                        self.request_quit();
                    }
                }
                KeyCode::Left | KeyCode::Right => {
                    self.focus = if self.focus == Focus::Settings {
                        Focus::Exit
                    } else {
                        Focus::Settings
                    };
                }
                KeyCode::Esc | KeyCode::Down => self.focus = Focus::Input,
                _ => {}
            },
            Focus::Results => self.on_results_key(key),
            Focus::Input => match key.code {
                KeyCode::Enter => self.submit(),
                KeyCode::Esc if self.downloading() => self.confirm_cancel_download(),
                KeyCode::Esc if self.searching() => self.stop_search(),
                KeyCode::Esc => {
                    if self.current_input().is_empty() && self.mode == Mode::Music {
                        self.search = None;
                    }
                    self.current_input_mut().clear();
                    self.notice = None;
                }
                KeyCode::Down if self.has_results() => self.focus = Focus::Results,
                KeyCode::Up => self.focus = Focus::Settings,
                KeyCode::Char('v') if ctrl => self.paste_into_input(),
                KeyCode::Insert if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.paste_into_input()
                }
                _ => {
                    if self.current_input_mut().handle_key(&key) {
                        self.notice = None;
                    }
                }
            },
        }
    }

    fn confirm_cancel_download(&mut self) {
        self.confirm_simple(
            "Отмена",
            "Прервать текущую загрузку?",
            Action::CancelDownload,
        );
    }

    fn on_results_key(&mut self, key: KeyEvent) {
        if !self.has_results() {
            self.focus = Focus::Input;
            return;
        }
        let Some(search) = self.search.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Up if search.selected == 0 => self.focus = Focus::Input,
            KeyCode::Up => search.move_selection(-1),
            KeyCode::Down => search.move_selection(1),
            KeyCode::PageUp => search.move_selection(-5),
            KeyCode::PageDown => search.move_selection(5),
            KeyCode::Home => search.selected = 0,
            KeyCode::End => search.move_selection(isize::MAX),
            KeyCode::Enter | KeyCode::Char(' ') => self.download_selected(),
            KeyCode::Esc if self.downloading() => self.confirm_cancel_download(),
            KeyCode::Esc => self.focus = Focus::Input,
            // Набор текста из списка — сразу в поле поиска.
            KeyCode::Char(_) | KeyCode::Backspace => {
                self.focus = Focus::Input;
                self.query.handle_key(&key);
            }
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        match self.modal.as_mut() {
            Some(Modal::Confirm(confirm)) => {
                let choice = match key.code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                        confirm.yes_selected = !confirm.yes_selected;
                        None
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => Some(confirm.yes_selected),
                    // Те же физические клавиши Y/N в русской раскладке — «н»/«т».
                    KeyCode::Char('y' | 'Y' | 'н' | 'Н') => Some(true),
                    KeyCode::Esc if confirm.esc_closes => {
                        self.modal = None;
                        return;
                    }
                    KeyCode::Char('n' | 'N' | 'т' | 'Т') | KeyCode::Esc => Some(false),
                    _ => None,
                };
                if let Some(yes) = choice {
                    self.resolve_confirm(yes);
                }
            }
            Some(Modal::Settings(_)) => self.on_settings_key(key),
            None => {}
        }
    }

    fn resolve_confirm(&mut self, yes: bool) {
        if let Some(Modal::Confirm(confirm)) = self.modal.take() {
            self.run_action(if yes { confirm.on_yes } else { confirm.on_no });
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) {
        let Some(Modal::Settings(form)) = self.modal.as_mut() else {
            return;
        };
        let fields = form.fields();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if !form.first_run => self.modal = None,
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % fields,
            KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + fields - 1) % fields,
            KeyCode::Left | KeyCode::Right if form.is_selector() => {
                let forward = key.code == KeyCode::Right;
                cycle_selector(form, forward);
            }
            KeyCode::Enter => match form.focus {
                field::PROXY => form.focus = field::FOLDER,
                field::FOLDER => form.focus = field::BROWSE,
                field::MUSIC_FOLDER => form.focus = field::MUSIC_BROWSE,
                _ => self.activate_settings_field(),
            },
            KeyCode::Char(' ') if !form.is_text_field() => self.activate_settings_field(),
            _ if form.is_text_field() => {
                let pasted = (ctrl && key.code == KeyCode::Char('v'))
                    || (key.code == KeyCode::Insert && key.modifiers.contains(KeyModifiers::SHIFT));
                if pasted {
                    if let Some(text) = clipboard_text() {
                        form.paste(&text);
                    }
                } else if form.focus == field::PROXY {
                    // В порт — только цифры; клавиши редактирования проходят как есть.
                    if !matches!(key.code, KeyCode::Char(c) if !c.is_ascii_digit()) {
                        form.proxy.handle_key(&key);
                    }
                } else if form.focus == field::FOLDER {
                    form.folder.handle_key(&key);
                } else {
                    form.music_folder.handle_key(&key);
                }
                form.error = None;
            }
            _ => {}
        }
    }

    fn activate_settings_field(&mut self) {
        let Some(Modal::Settings(form)) = self.modal.as_mut() else {
            return;
        };
        match form.focus {
            field::QUALITY | field::AUDIO | field::THEME | field::BROWSER => {
                cycle_selector(form, true)
            }
            field::BROWSE | field::MUSIC_BROWSE => {
                let (input, title) = if form.focus == field::BROWSE {
                    (&mut form.folder, "Папка для сохранения видео")
                } else {
                    (&mut form.music_folder, "Папка для сохранения музыки")
                };
                // Нативный диалог ОС; TUI на это время замирает.
                let current = PathBuf::from(input.value());
                let mut dialog = rfd::FileDialog::new().set_title(title);
                if current.is_dir() {
                    dialog = dialog.set_directory(&current);
                }
                if let Some(dir) = dialog.pick_folder() {
                    input.set(dir.to_string_lossy());
                    form.error = None;
                }
            }
            field::SAVE => self.save_settings(),
            field::CANCEL => self.modal = None,
            _ => {}
        }
    }

    fn save_settings(&mut self) {
        let Some(Modal::Settings(form)) = self.modal.as_mut() else {
            return;
        };
        let dir = match checked_dir(&form.folder) {
            Ok(dir) => dir,
            Err(e) => {
                form.error = Some(e);
                form.focus = field::FOLDER;
                return;
            }
        };
        let music_dir = match checked_dir(&form.music_folder) {
            Ok(dir) => dir,
            Err(e) => {
                form.error = Some(e);
                form.focus = field::MUSIC_FOLDER;
                return;
            }
        };
        let port = form.proxy.value().trim();
        let proxy_port = if port.is_empty() {
            None
        } else {
            match port.parse::<u16>() {
                Ok(p) if p > 0 => Some(p),
                _ => {
                    form.error = Some("Порт прокси — число от 1 до 65535 (или пусто)".into());
                    form.focus = field::PROXY;
                    return;
                }
            }
        };
        let config = Config {
            download_dir: dir,
            music_dir,
            quality: Quality::ALL[form.quality],
            audio_format: AudioFormat::ALL[form.audio],
            browser: Browser::ALL[form.browser],
            proxy_port,
            theme: Theme::ALL[form.theme],
        };
        if let Err(e) = config.save() {
            form.error = Some(format!("Не удалось сохранить настройки: {e}"));
            return;
        }
        let first_run = form.first_run;
        let browser_changed = config.browser != self.config.browser;
        self.config = config;
        self.modal = None;
        if browser_changed {
            // Старый файл — от другого браузера, использовать его нельзя.
            let _ = fs::remove_file(config::cookies_path());
            self.cookies_refreshed = false;
        }
        if first_run {
            self.first_run = false;
            self.advance_startup();
        } else if browser_changed && (self.searching() || !self.busy()) {
            // Поиск не жалко прервать: cookie нужны для следующих запросов.
            self.stop_search();
            self.advance_startup();
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        let scroll = match mouse.kind {
            MouseEventKind::ScrollUp => Some(-1),
            MouseEventKind::ScrollDown => Some(1),
            _ => None,
        };
        if let Some(delta) = scroll {
            if self.modal.is_none()
                && self.has_results()
                && self.hits.results.contains(pos)
                && let Some(search) = self.search.as_mut()
            {
                search.move_selection(delta);
            }
            return;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        if let Stage::Fatal(_) = self.stage {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            let hit = self
                .hits
                .modal
                .iter()
                .find(|(r, _)| r.contains(pos))
                .map(|(_, i)| *i);
            let Some(index) = hit else { return };
            match self.modal.as_mut() {
                Some(Modal::Confirm(_)) => self.resolve_confirm(index == 0),
                Some(Modal::Settings(form)) => {
                    form.focus = index;
                    if !form.is_text_field() {
                        self.activate_settings_field();
                    }
                }
                None => {}
            }
            return;
        }
        if self.hits.exit.contains(pos) {
            self.request_quit();
            return;
        }
        if matches!(self.stage, Stage::Startup) {
            // Пока идёт подготовка, доступен только выход.
            return;
        }
        let mode = self
            .hits
            .modes
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, m)| *m);
        let row = self
            .hits
            .result_rows
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, i)| *i);
        if let Some(mode) = mode {
            if mode != self.mode {
                self.switch_mode();
            }
        } else if let Some(index) = row {
            // Первый клик выбирает трек, клик по выбранному — скачивает.
            let Some(search) = self.search.as_mut() else {
                return;
            };
            let again = self.focus == Focus::Results && search.selected == index;
            search.selected = index;
            self.focus = Focus::Results;
            if again {
                self.download_selected();
            }
        } else if self.hits.settings.contains(pos) {
            self.open_settings(false);
        } else if self.hits.input.contains(pos) {
            self.focus = Focus::Input;
        } else if self.hits.open_folder.contains(pos) {
            self.open_download_folder();
        }
    }
}

/// Папка из поля настроек: не пустая и создаётся.
fn checked_dir(input: &Input) -> Result<PathBuf, String> {
    let folder = input.value().trim().trim_matches('"');
    if folder.is_empty() {
        return Err("Укажите папку для сохранения".into());
    }
    let dir = PathBuf::from(folder);
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать папку: {e}"))?;
    Ok(dir)
}

fn cycle_selector(form: &mut SettingsForm, forward: bool) {
    let (value, len) = match form.focus {
        field::QUALITY => (&mut form.quality, Quality::ALL.len()),
        field::AUDIO => (&mut form.audio, AudioFormat::ALL.len()),
        field::THEME => (&mut form.theme, Theme::ALL.len()),
        field::BROWSER => (&mut form.browser, Browser::ALL.len()),
        _ => return,
    };
    *value = if forward {
        (*value + 1) % len
    } else {
        (*value + len - 1) % len
    };
}

fn push_log(log: &mut VecDeque<String>, line: String) {
    log.push_back(line);
    while log.len() > LOG_LIMIT {
        log.pop_front();
    }
}

fn describe_app(app: &str) -> &'static str {
    match app {
        "yt-dlp" => "загрузчик видео",
        "ffmpeg" => "склейка видео и звука",
        "deno" => "JS-движок, без него YouTube отдаёт не все форматы",
        "git" => "нужен scoop для обновлений",
        _ => "",
    }
}
