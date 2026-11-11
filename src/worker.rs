//! Фоновые задачи: запуск внешних процессов с потоковым чтением вывода.

use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use crate::{download::DownloadSummary, music::Track, packages::AppUpdate};

/// Дочерний процесс получает собственную скрытую консоль и не рисует поверх TUI.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const CANCELLED: &str = "Отменено пользователем";

#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// 0.0..=1.0, `None` — размер неизвестен.
    pub ratio: Option<f64>,
    pub detail: String,
}

#[derive(Debug)]
pub enum Event {
    Log(String),
    Step(String),
    Progress(Progress),
    Item {
        index: usize,
        total: usize,
    },
    /// Очередной результат поиска в YouTube Music.
    Track(Track),
    Finished(Result<Outcome, String>),
}

#[derive(Debug)]
pub enum Outcome {
    Done,
    Updates(Vec<AppUpdate>),
    Download(DownloadSummary),
}

/// Контекст, передаваемый в тело фоновой задачи.
#[derive(Clone)]
pub struct Ctx {
    tx: Sender<Event>,
    cancel: Arc<AtomicBool>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl Ctx {
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn log(&self, line: impl Into<String>) {
        self.send(Event::Log(line.into()));
    }

    pub fn step(&self, text: impl Into<String>) {
        self.send(Event::Step(text.into()));
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn for_test(tx: Sender<Event>) -> Ctx {
        Ctx {
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(Mutex::new(None)),
        }
    }
}

pub struct Worker {
    pub rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl Worker {
    pub fn spawn<F>(job: F) -> Worker
    where
        F: FnOnce(&Ctx) -> Result<Outcome, String> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let ctx = Ctx {
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(Mutex::new(None)),
        };
        let worker = Worker {
            rx,
            cancel: ctx.cancel.clone(),
            pid: ctx.pid.clone(),
        };
        thread::spawn(move || {
            let result = job(&ctx);
            let result = if ctx.cancelled() {
                Err(CANCELLED.to_string())
            } else {
                result
            };
            ctx.send(Event::Finished(result));
        });
        worker
    }

    /// Просит задачу остановиться и убивает текущий процесс вместе с потомками
    /// (yt-dlp запускает ffmpeg, PyInstaller-сборка — ещё один процесс).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap() {
            kill_tree(pid);
        }
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    isolate(&mut cmd);
    let _ = cmd.status();
}

/// Процесс запущен в собственной группе (`isolate`), её id равен pid —
/// сигнал группе убивает и его потомков.
#[cfg(unix)]
fn kill_tree(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid) {
        // SAFETY: kill(2) только отправляет сигнал, память не затрагивает.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Windows: своя скрытая консоль, чтобы процесс не рисовал поверх TUI.
/// Unix: своя группа процессов, чтобы при отмене убить и потомков (ffmpeg).
pub fn isolate(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

/// Запускает процесс и построчно отдаёт stdout/stderr в `on_line(line, is_stderr)`.
/// Строки режутся и по `\n`, и по `\r` — так прогресс-бары консольных утилит
/// превращаются в отдельные обновления.
pub fn run(
    ctx: &Ctx,
    mut cmd: Command,
    mut on_line: impl FnMut(&str, bool),
) -> Result<ExitStatus, String> {
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    isolate(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Не удалось запустить {program}: {e}"))?;
    *ctx.pid.lock().unwrap() = Some(child.id());

    let (line_tx, line_rx) = mpsc::channel();
    let readers = [
        spawn_reader(
            child.stdout.take().expect("stdout piped"),
            false,
            line_tx.clone(),
        ),
        spawn_reader(child.stderr.take().expect("stderr piped"), true, line_tx),
    ];
    // Канал закрывается, когда оба читателя дошли до EOF.
    for (line, is_err) in line_rx {
        on_line(&line, is_err);
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .map_err(|e| format!("Ошибка ожидания {program}: {e}"));
    *ctx.pid.lock().unwrap() = None;
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    status
}

fn spawn_reader(
    mut source: impl Read + Send + 'static,
    is_err: bool,
    tx: Sender<(String, bool)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        let mut line = Vec::new();
        let flush = |line: &mut Vec<u8>| {
            if !line.is_empty() {
                let text = String::from_utf8_lossy(line).trim_end().to_string();
                line.clear();
                if !text.is_empty() {
                    let _ = tx.send((text, is_err));
                }
            }
        };
        loop {
            match source.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &chunk[..n] {
                        if b == b'\n' || b == b'\r' {
                            flush(&mut line);
                        } else {
                            line.push(b);
                        }
                    }
                }
            }
        }
        flush(&mut line);
    })
}
