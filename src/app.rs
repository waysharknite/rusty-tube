//! Состояние приложения, сценарий запуска и обработка ввода.

use std::{
    cell::Cell,
    collections::VecDeque,
    fs,
    path::PathBuf,
    process::Command,
    sync::mpsc::{Receiver, TryRecvError},
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
    input::{Input, clipboard_text, latin_shortcut},
    install,
    music::{self, Track},
    packages::{self, AppUpdate, Packages},
    release::{self, Release},
    timecode::{self, Bound, Section},
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
    /// Заголовок раскрывающегося блока «Параметры загрузки» (режим видео).
    Options,
    /// Выбор качества видео.
    Quality,
    /// Галочка «Конвертация в MP3».
    Mp3,
    /// Выбор «Полностью» / «Участок».
    Range,
    /// Поля промежутка «от» и «до» (видны, когда качается участок).
    From,
    To,
    /// Галочка «Субтитры».
    Subtitles,
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
    /// Начать загрузку: ссылка, «весь плейлист или только видео», что качать, участок
    /// и вжигать ли субтитры.
    Download(String, bool, Kind, Option<Section>, bool),
    /// Глобальная установка (кнопка в настройках).
    InstallGlobal,
    /// Открыть страницу релиза новой версии.
    OpenRelease(String),
    /// Не напоминать об этой версии программы.
    SkipVersion(String),
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
    pub const AUDIO: usize = 0;
    pub const THEME: usize = 1;
    pub const BROWSER: usize = 2;
    pub const NO_COOKIES: usize = 3;
    pub const PROXY: usize = 4;
    pub const FOLDER: usize = 5;
    pub const BROWSE: usize = 6;
    pub const MUSIC_FOLDER: usize = 7;
    pub const MUSIC_BROWSE: usize = 8;
    pub const MP3_FOLDER: usize = 9;
    pub const MP3_BROWSE: usize = 10;
    pub const NO_UPDATES: usize = 11;
    pub const GLOBAL: usize = 12;
    pub const SAVE: usize = 13;
    pub const CANCEL: usize = 14;
}

pub struct SettingsForm {
    pub audio: usize,
    /// Пока окно открыто, интерфейс рисуется выбранной здесь темой (предпросмотр).
    pub theme: usize,
    pub browser: usize,
    /// Галочка «Не запрашивать cookie».
    pub no_cookies: bool,
    /// Галочка «Не проверять обновления».
    pub no_update_check: bool,
    /// Порт SOCKS5-прокси; пусто — без прокси.
    pub proxy: Input,
    pub folder: Input,
    pub music_folder: Input,
    /// Пусто — MP3 сохраняются в папку видео.
    pub mp3_folder: Input,
    pub focus: usize,
    /// Первичная настройка: отменить нельзя, только сохранить.
    pub first_run: bool,
    pub error: Option<String>,
    /// Сообщение об успехе (показывается на месте ошибки).
    pub info: Option<String>,
    /// Программа уже установлена для запуска из любой папки.
    pub installed: bool,
    /// Прокрутка содержимого, когда окно не помещается в терминал. Пределы и
    /// следование за фокусом считаются при отрисовке, поэтому `Cell`.
    pub scroll: Cell<u16>,
    /// Поле, к которому прокрутка подстроилась в последний раз.
    pub scrolled_to: Cell<Option<usize>>,
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
            field::PROXY | field::FOLDER | field::MUSIC_FOLDER | field::MP3_FOLDER
        )
    }

    fn is_selector(&self) -> bool {
        matches!(self.focus, field::AUDIO | field::THEME | field::BROWSER)
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
            field::MP3_FOLDER => self.mp3_folder.insert_str(text.trim()),
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
    /// Элементы блока «Параметры загрузки».
    pub controls: Vec<(Rect, Focus)>,
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
    /// Блок «Параметры загрузки» раскрыт.
    pub options_open: bool,
    /// Режим видео: качать целиком или только участок `range_from`..`range_to`.
    pub full_video: bool,
    /// Режим видео: вжечь в видео русские (или английские) субтитры.
    pub subtitles: bool,
    /// Режим видео: качать только звук и перекодировать в MP3.
    pub to_mp3: bool,
    pub range_from: Input,
    pub range_to: Input,
    pub focus: Focus,
    pub download: Option<DownloadView>,
    pub search: Option<SearchView>,
    pub notice: Option<String>,
    /// Cookie не удалось обновить — показывается справа в строке состояния.
    pub cookie_warning: Option<String>,
    pub hits: Hits,
    pub tick: usize,
    worker: Option<(Job, Worker)>,
    updates_checked: bool,
    /// Cookie уже экспортированы из браузера в этом запуске (или это не требуется).
    cookies_refreshed: bool,
    /// Окно настроек, спрятанное на время вопроса из него; возвращается после ответа.
    stashed_settings: Option<SettingsForm>,
    /// Фоновая проверка новой версии на GitHub; `None` — уже показали или нечего.
    release: Option<Receiver<Release>>,
    quit: bool,
}

impl App {
    pub fn new() -> App {
        let loaded = Config::load();
        let release = match &loaded {
            Some(c) if c.no_update_check => None,
            _ => Some(release::check_in_background(
                loaded.as_ref().and_then(|c| c.skipped_version.clone()),
            )),
        };
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
            options_open: false,
            full_video: true,
            subtitles: false,
            to_mp3: false,
            range_from: Input::default(),
            range_to: Input::default(),
            focus: Focus::Input,
            download: None,
            search: None,
            notice: None,
            cookie_warning: None,
            hits: Hits::default(),
            tick: 0,
            worker: None,
            updates_checked: false,
            cookies_refreshed: false,
            stashed_settings: None,
            release,
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
            self.pump_release();
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

    /// Строки блока «Параметры загрузки» сверху вниз (только режим видео),
    /// включая недоступные сейчас элементы.
    pub fn option_rows(&self) -> Vec<Vec<Focus>> {
        if self.mode != Mode::Video {
            return Vec::new();
        }
        let mut rows = vec![vec![Focus::Options]];
        if self.options_open {
            rows.extend([vec![Focus::Quality], vec![Focus::Mp3], vec![Focus::Range]]);
            if !self.full_video {
                rows.push(vec![Focus::From, Focus::To]);
            }
            rows.push(vec![Focus::Subtitles]);
        }
        rows
    }

    /// Качество и субтитры — только для видео, при конвертации в MP3 они не нужны.
    pub fn control_enabled(&self, control: Focus) -> bool {
        !(self.to_mp3 && matches!(control, Focus::Quality | Focus::Subtitles))
    }

    /// Доступные элементы под полем ссылки в порядке обхода.
    pub fn video_controls(&self) -> Vec<Focus> {
        self.option_rows()
            .into_iter()
            .flatten()
            .filter(|c| self.control_enabled(*c))
            .collect()
    }

    /// Что качается в режиме видео при текущих галочках.
    pub fn video_kind(&self) -> Kind {
        if self.to_mp3 { Kind::Mp3 } else { Kind::Video }
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

        if !self.updates_checked && !self.config.no_update_check {
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

        let browser = self.config.cookie_browser();
        if !self.cookies_refreshed && browser != Browser::None {
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
                            Kind::Video | Kind::Mp3 => "Видео",
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
                    self.cookie_warning = Some(format!("Cookie не обновлены ({fallback}): {e}"));
                } else {
                    self.cookie_warning = None;
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
            Action::Download(url, playlist, kind, section, subtitles) => {
                self.clear_input(kind);
                self.start_download(url, playlist, kind, None, section, subtitles);
            }
            Action::InstallGlobal => {
                let result = install::install();
                if let Some(form) = self.stashed_settings.as_mut() {
                    match result {
                        Ok(message) => {
                            form.info = Some(message);
                            form.error = None;
                        }
                        Err(e) => {
                            form.error = Some(e);
                            form.info = None;
                        }
                    }
                    form.installed = install::is_installed();
                }
            }
            Action::OpenRelease(url) => release::open_in_browser(&url),
            Action::SkipVersion(tag) => {
                self.config.skipped_version = Some(tag);
                if let Err(e) = self.config.save() {
                    self.notice = Some(format!("Не удалось сохранить настройки: {e}"));
                }
            }
            Action::Nothing => {}
        }
    }

    /// Показывает окно о новой версии, когда программа готова и других окон нет.
    fn pump_release(&mut self) {
        if !matches!(self.stage, Stage::Ready)
            || self.modal.is_some()
            || self.stashed_settings.is_some()
        {
            return;
        }
        let Some(rx) = &self.release else {
            return;
        };
        let release = match rx.try_recv() {
            Ok(release) => release,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.release = None;
                return;
            }
        };
        self.release = None;
        self.modal = Some(Modal::Confirm(Confirm {
            title: "Новая версия".into(),
            body: vec![
                format!(
                    "Вышла новая версия rusty-tube: {} (у вас {}).",
                    release.tag,
                    env!("CARGO_PKG_VERSION")
                ),
                String::new(),
                "«Скачать» откроет страницу релиза в браузере.".into(),
                "Esc — напомнить при следующем запуске.".into(),
            ],
            yes: "Скачать",
            no: "Пропустить эту версию",
            yes_selected: true,
            on_yes: Action::OpenRelease(release.url),
            on_no: Action::SkipVersion(release.tag),
            esc_closes: true,
        }));
    }

    /// Кнопка «Установить» в настройках: спрашиваем, затем возвращаемся в настройки.
    fn confirm_install_global(&mut self) {
        let Some(Modal::Settings(form)) = self.modal.take() else {
            return;
        };
        self.stashed_settings = Some(form);
        let mut body = vec![
            "Запускайте rusty-tube из любой папки, сделать изменения?".to_string(),
            String::new(),
        ];
        body.extend(install::plan().describe());
        self.modal = Some(Modal::Confirm(Confirm {
            title: "Глобальная установка".into(),
            body,
            yes: "Да",
            no: "Нет",
            yes_selected: true,
            on_yes: Action::InstallGlobal,
            on_no: Action::Nothing,
            esc_closes: false,
        }));
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
            audio: self.config.audio_format.index(),
            theme: self.config.theme.index(),
            browser: self.config.browser.index(),
            no_cookies: self.config.no_cookies,
            no_update_check: self.config.no_update_check,
            proxy: Input::new(
                self.config
                    .proxy_port
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            folder: Input::new(self.config.download_dir.to_string_lossy()),
            music_folder: Input::new(self.config.music_dir.to_string_lossy()),
            mp3_folder: Input::new(
                self.config
                    .mp3_dir
                    .as_deref()
                    .map(|dir| dir.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            focus: field::AUDIO,
            first_run,
            error: None,
            info: None,
            installed: install::is_installed(),
            scroll: Cell::new(0),
            scrolled_to: Cell::new(None),
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
        if self.input.value().trim().is_empty() {
            self.notice = Some("Вставьте ссылку на видео".into());
            self.focus = Focus::Input;
            return;
        }
        let Some(url) = download::normalize_url(self.input.value()) else {
            self.notice = Some("Это не похоже на ссылку YouTube".into());
            self.focus = Focus::Input;
            return;
        };
        let section = if self.full_video {
            None
        } else {
            match timecode::parse_section(self.range_from.value(), self.range_to.value()) {
                Ok(section) => section,
                Err(e) => {
                    self.notice = Some(e.message);
                    self.focus = match e.bound {
                        Bound::From => Focus::From,
                        Bound::To => Focus::To,
                    };
                    return;
                }
            }
        };
        let kind = self.video_kind();
        let subtitles = self.subtitles && kind == Kind::Video;
        self.begin_download(url, kind, section, subtitles);
    }

    /// Enter в режиме музыки: ссылку качаем сразу, остальное ищем.
    fn submit_query(&mut self) {
        let text = self.query.value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(url) = download::normalize_url(&text) {
            self.begin_download(url, Kind::Audio, None, false);
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
    fn begin_download(
        &mut self,
        url: String,
        kind: Kind,
        section: Option<Section>,
        subtitles: bool,
    ) {
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
                Kind::Video | Kind::Mp3 => ("видео", "только это видео"),
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
                    Kind::Video | Kind::Mp3 => "Только это видео",
                    Kind::Audio => "Только этот трек",
                },
                yes_selected: false,
                on_yes: Action::Download(url.clone(), true, kind, section, subtitles),
                on_no: Action::Download(url, false, kind, section, subtitles),
                esc_closes: true,
            }));
            return;
        }
        let playlist = download::is_playlist(&url) || music::is_album(&url);
        self.clear_input(kind);
        self.start_download(url, playlist, kind, None, section, subtitles);
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
        self.start_download(track.url(), false, Kind::Audio, Some(label), None, false);
    }

    fn dir_for(&self, kind: Kind) -> PathBuf {
        match kind {
            Kind::Video => self.config.download_dir.clone(),
            Kind::Audio => self.config.music_dir.clone(),
            Kind::Mp3 => self.config.mp3_folder().to_path_buf(),
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
            Kind::Video | Kind::Mp3 => self.input.clear(),
            Kind::Audio => self.query.clear(),
        }
    }

    fn start_download(
        &mut self,
        url: String,
        playlist: bool,
        kind: Kind,
        label: Option<String>,
        section: Option<Section>,
        subtitles: bool,
    ) {
        let mut label = label.unwrap_or_else(|| url.clone());
        if kind == Kind::Mp3 {
            label.push_str(" · MP3");
        }
        if let Some(section) = &section {
            label.push_str(&format!(" · фрагмент {}", section.label()));
        }
        if subtitles {
            label.push_str(" · субтитры");
        }
        self.download = Some(DownloadView {
            label,
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
            section,
            subtitles,
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
                Mode::Video => self.video_kind(),
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

    fn range_input_mut(&mut self) -> Option<&mut Input> {
        match self.focus {
            Focus::From => Some(&mut self.range_from),
            Focus::To => Some(&mut self.range_to),
            _ => None,
        }
    }

    /// Вставка в поле времени: только цифры, «:» и «.».
    fn paste_time(&mut self, text: &str) {
        let filtered: String = text
            .trim()
            .chars()
            .filter_map(timecode::time_char)
            .collect();
        if let Some(input) = self.range_input_mut() {
            input.insert_str(&filtered);
        }
        self.notice = None;
    }

    /// Пробел, клик или ←→ на элементе блока параметров: переключить или выбрать
    /// следующее (`forward`) или предыдущее значение.
    fn activate(&mut self, control: Focus, forward: bool) {
        if !self.control_enabled(control) {
            return;
        }
        match control {
            Focus::Options => self.options_open = !self.options_open,
            Focus::Mp3 => self.to_mp3 = !self.to_mp3,
            Focus::Range => self.full_video = !self.full_video,
            Focus::Subtitles => self.subtitles = !self.subtitles,
            Focus::Quality => {
                let n = Quality::ALL.len();
                let i = self.config.quality.index();
                let i = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.config.quality = Quality::ALL[i];
                // Выбор качества запоминается между запусками, как раньше в настройках.
                if let Err(e) = self.config.save() {
                    self.notice = Some(format!("Не удалось сохранить настройки: {e}"));
                    return;
                }
            }
            _ => return,
        }
        self.notice = None;
    }

    /// ↑↓ по строкам блока параметров; выше первой строки — поле ссылки.
    fn move_option_row(&mut self, down: bool) {
        let rows: Vec<Vec<Focus>> = self
            .option_rows()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter(|c| self.control_enabled(*c))
                    .collect::<Vec<_>>()
            })
            .filter(|row| !row.is_empty())
            .collect();
        let Some((r, c)) = rows
            .iter()
            .enumerate()
            .find_map(|(r, row)| row.iter().position(|f| *f == self.focus).map(|c| (r, c)))
        else {
            self.focus = Focus::Input;
            return;
        };
        if down {
            if let Some(row) = rows.get(r + 1) {
                self.focus = row[c.min(row.len() - 1)];
            }
        } else if r == 0 {
            self.focus = Focus::Input;
        } else {
            let row = &rows[r - 1];
            self.focus = row[c.min(row.len() - 1)];
        }
    }

    // ---------------------------------------------------------------- ввод

    fn on_event(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.on_key(latin_shortcut(key))
            }
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::Paste(text) => self.paste(&text),
            _ => {}
        }
    }

    /// Вставка текста целиком: из терминала (bracketed paste) или правым кликом.
    fn paste(&mut self, text: &str) {
        match &mut self.modal {
            Some(Modal::Settings(form)) => form.paste(text),
            None if matches!(self.focus, Focus::From | Focus::To) => self.paste_time(text),
            None => {
                self.focus = Focus::Input;
                self.current_input_mut().insert_str(text.trim());
            }
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
                order.extend(self.video_controls());
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
            Focus::Options | Focus::Quality | Focus::Mp3 | Focus::Range | Focus::Subtitles => {
                self.on_option_key(key)
            }
            Focus::From | Focus::To => self.on_range_key(key),
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
                KeyCode::Down if self.mode == Mode::Video => self.focus = Focus::Options,
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

    /// Блок параметров: пробел — переключить, ←→ — значение, Enter — скачать
    /// (на заголовке — раскрыть или свернуть).
    fn on_option_key(&mut self, key: KeyEvent) {
        if self.mode != Mode::Video {
            self.focus = Focus::Input;
            return;
        }
        let selector = matches!(self.focus, Focus::Quality | Focus::Range);
        let header = self.focus == Focus::Options;
        match key.code {
            KeyCode::Char(' ') => self.activate(self.focus, true),
            KeyCode::Enter if header => self.activate(Focus::Options, true),
            KeyCode::Enter => self.submit(),
            KeyCode::Left | KeyCode::Right if selector => {
                self.activate(self.focus, key.code == KeyCode::Right)
            }
            // На заголовке: → раскрыть, ← свернуть.
            KeyCode::Right if header => self.options_open = true,
            KeyCode::Left if header => self.options_open = false,
            KeyCode::Up => self.move_option_row(false),
            KeyCode::Down => self.move_option_row(true),
            KeyCode::Esc if self.downloading() => self.confirm_cancel_download(),
            KeyCode::Esc => self.focus = Focus::Input,
            _ => {}
        }
    }

    /// Поля «от» и «до»: ввод времени, Enter — скачать.
    fn on_range_key(&mut self, key: KeyEvent) {
        if self.mode != Mode::Video || self.full_video || !self.options_open {
            self.focus = Focus::Input;
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let pasted = (ctrl && key.code == KeyCode::Char('v'))
            || (key.code == KeyCode::Insert && key.modifiers.contains(KeyModifiers::SHIFT));
        match key.code {
            _ if pasted => {
                if let Some(text) = clipboard_text() {
                    self.paste_time(&text);
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Up => self.move_option_row(false),
            KeyCode::Down => self.move_option_row(true),
            KeyCode::Esc if self.downloading() => self.confirm_cancel_download(),
            KeyCode::Esc => self.focus = Focus::Input,
            KeyCode::Char(c) if !ctrl => {
                if let Some(c) = timecode::time_char(c)
                    && let Some(input) = self.range_input_mut()
                {
                    input.insert_str(&c.to_string());
                    self.notice = None;
                }
            }
            _ => {
                if let Some(input) = self.range_input_mut()
                    && input.handle_key(&key)
                {
                    self.notice = None;
                }
            }
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
        if self.modal.is_none()
            && let Some(form) = self.stashed_settings.take()
        {
            self.modal = Some(Modal::Settings(form));
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
                field::MP3_FOLDER => form.focus = field::MP3_BROWSE,
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
                } else if form.focus == field::MUSIC_FOLDER {
                    form.music_folder.handle_key(&key);
                } else {
                    form.mp3_folder.handle_key(&key);
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
            field::AUDIO | field::THEME | field::BROWSER => cycle_selector(form, true),
            field::BROWSE | field::MUSIC_BROWSE | field::MP3_BROWSE => {
                let video_folder = PathBuf::from(form.folder.value());
                let (input, title) = match form.focus {
                    field::BROWSE => (&mut form.folder, "Папка для сохранения видео"),
                    field::MUSIC_BROWSE => (&mut form.music_folder, "Папка для сохранения музыки"),
                    _ => (&mut form.mp3_folder, "Папка для сохранения MP3"),
                };
                // Нативный диалог ОС; TUI на это время замирает.
                // Пустая папка MP3 означает папку видео — с неё и начинаем.
                let current = match input.value() {
                    "" => video_folder,
                    value => PathBuf::from(value),
                };
                let mut dialog = rfd::FileDialog::new().set_title(title);
                if current.is_dir() {
                    dialog = dialog.set_directory(&current);
                }
                if let Some(dir) = dialog.pick_folder() {
                    input.set(dir.to_string_lossy());
                    form.error = None;
                }
            }
            field::NO_COOKIES => form.no_cookies = !form.no_cookies,
            field::NO_UPDATES => form.no_update_check = !form.no_update_check,
            field::GLOBAL => self.confirm_install_global(),
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
        // Папка MP3 необязательна: пусто — сохранять туда же, куда видео.
        let mp3_dir = if clean_folder(&form.mp3_folder).is_empty() {
            None
        } else {
            match checked_dir(&form.mp3_folder) {
                Ok(dir) => Some(dir),
                Err(e) => {
                    form.error = Some(e);
                    form.focus = field::MP3_FOLDER;
                    return;
                }
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
            mp3_dir,
            quality: self.config.quality,
            audio_format: AudioFormat::ALL[form.audio],
            browser: Browser::ALL[form.browser],
            no_cookies: form.no_cookies,
            no_update_check: form.no_update_check,
            proxy_port,
            theme: Theme::ALL[form.theme],
            skipped_version: self.config.skipped_version.clone(),
        };
        if let Err(e) = config.save() {
            form.error = Some(format!("Не удалось сохранить настройки: {e}"));
            return;
        }
        let first_run = form.first_run;
        // Сменился источник cookie: другой браузер или галочка «Не запрашивать».
        let browser_changed = config.cookie_browser() != self.config.cookie_browser();
        self.config = config;
        self.modal = None;
        if browser_changed {
            self.cookie_warning = None;
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
            if let Some(Modal::Settings(form)) = self.modal.as_ref() {
                // Верхний предел прокрутки знает только отрисовка — она и подрежет.
                let value = form.scroll.get().saturating_add_signed(delta as i16 * 2);
                form.scroll.set(value);
            } else if self.modal.is_none()
                && self.has_results()
                && self.hits.results.contains(pos)
                && let Some(search) = self.search.as_mut()
            {
                search.move_selection(delta);
            }
            return;
        }
        // Правый клик вставляет, как в обычном окне cmd: захват мыши отключает там
        // встроенную вставку правой кнопкой.
        if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
            if matches!(self.stage, Stage::Ready)
                && let Some(text) = clipboard_text()
            {
                self.paste(&text);
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
        let control = self
            .hits
            .controls
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, f)| *f);
        if let Some(mode) = mode {
            if mode != self.mode {
                self.switch_mode();
            }
        } else if let Some(control) = control {
            if self.control_enabled(control) {
                self.focus = control;
                self.activate(control, true);
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
fn clean_folder(input: &Input) -> &str {
    input.value().trim().trim_matches('"').trim()
}

fn checked_dir(input: &Input) -> Result<PathBuf, String> {
    let folder = clean_folder(input);
    if folder.is_empty() {
        return Err("Укажите папку для сохранения".into());
    }
    let dir = PathBuf::from(folder);
    fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать папку: {e}"))?;
    Ok(dir)
}

fn cycle_selector(form: &mut SettingsForm, forward: bool) {
    let (value, len) = match form.focus {
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
