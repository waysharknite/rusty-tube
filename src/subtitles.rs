//! Субтитры, вжигаемые в видео: загрузка через yt-dlp и рендер через ffmpeg.
//!
//! Видео качается в файл с меткой `.rt-subs`, его id и путь yt-dlp дописывает в
//! список (`--print-to-file`). Потом отдельным вызовом yt-dlp качаются субтитры
//! (`--skip-download`), чтобы их ошибка (например, 429 на автопереводе) не роняла
//! загрузку видео. Субтитры берутся в json3: в vtt автосубтитры YouTube идут
//! «лесенкой» с повторами строк, а из json3 получается чистый SRT.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::{
    timecode::Section,
    worker::{self, Ctx, Progress},
};

/// Метка в имени скачанного, но ещё не обработанного видео.
pub const MARKER: &str = ".rt-subs";
/// Языки по убыванию приоритета: русские, а если их нет — английские.
pub const LANGS: [&str; 2] = ["ru", "en"];
/// Список «id путь» скачанных видео во временной папке.
pub const LIST_FILE: &str = "files.txt";
/// Имя SRT во временной папке: ffmpeg запускается из неё, поэтому путь в фильтре
/// `subtitles=` без двоеточий и обратных слешей, которые пришлось бы экранировать.
const SRT_FILE: &str = "subs.srt";

/// Аргументы основной загрузки: записать id и итоговый путь каждого видео.
pub fn add_list_args(cmd: &mut Command, tmp: &Path) {
    cmd.args(["--print-to-file", "after_move:%(id)s %(filepath)s"])
        .arg(tmp.join(LIST_FILE));
}

/// Временная папка загрузки с субтитрами; удаляется при удалении значения.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn create() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir =
            std::env::temp_dir().join(format!("rusty-tube-subs-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Всё, что нужно для загрузки субтитров и рендера.
pub struct Job<'a> {
    pub yt_dlp: &'a Path,
    pub ffmpeg_dir: &'a Path,
    pub path_env: &'a OsString,
    pub cookies: Option<&'a Path>,
    pub proxy: Option<&'a str>,
    pub section: Option<Section>,
    pub tmp: &'a Path,
}

/// Качает субтитры к скачанным видео и вжигает их. Видео без субтитров (или если
/// рендер не удался) просто теряют метку в имени. `files` — пути из сводки загрузки,
/// обработанные заменяются итоговыми.
pub fn process(ctx: &Ctx, job: &Job, files: &mut [String], errors: &mut Vec<String>) {
    let videos = read_list(&job.tmp.join(LIST_FILE));
    if videos.is_empty() {
        return;
    }
    if !ctx.cancelled() {
        fetch(ctx, job, &videos);
    }
    for (id, path) in &videos {
        let result = if ctx.cancelled() {
            None
        } else {
            match pick_subtitles(job.tmp, id).and_then(|(lang, file)| {
                let srt = to_srt(&fs::read_to_string(file).ok()?, job.section)?;
                Some((lang, srt))
            }) {
                None => {
                    ctx.log(format!("Субтитров нет: {}", file_name(path)));
                    None
                }
                Some((lang, srt)) => match burn(ctx, job, path, lang, &srt) {
                    Ok(out) => Some(out),
                    Err(e) => {
                        let message = format!("субтитры не встроены в {}: {e}", file_name(path));
                        ctx.log(format!("Ошибка: {message}"));
                        errors.push(message);
                        None
                    }
                },
            }
        };
        let final_path = match result {
            Some(out) => {
                let _ = fs::remove_file(path);
                out
            }
            None => {
                let target = unmarked(path, None);
                match fs::rename(path, &target) {
                    Ok(()) => target,
                    Err(_) => PathBuf::from(path),
                }
            }
        };
        let final_path = final_path.to_string_lossy().into_owned();
        for file in files.iter_mut() {
            if Path::new(file.as_str()) == Path::new(path) {
                file.clone_from(&final_path);
            }
        }
    }
}

/// Строки `id путь` из списка, без повторов (видео могло встретиться дважды).
fn read_list(file: &Path) -> Vec<(String, String)> {
    let text = fs::read_to_string(file).unwrap_or_default();
    let mut videos: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some((id, path)) = line.trim_end().split_once(' ')
            && !videos.iter().any(|(_, p)| p == path)
        {
            videos.push((id.to_string(), path.to_string()));
        }
    }
    videos
}

/// `yt-dlp --skip-download --write-subs --write-auto-subs` для всех видео разом.
/// Ошибки не фатальны: у видео просто не будет субтитров.
fn fetch(ctx: &Ctx, job: &Job, videos: &[(String, String)]) {
    ctx.step("Загрузка субтитров");
    ctx.send(worker::Event::Progress(Progress::default()));
    let mut cmd = Command::new(job.yt_dlp);
    cmd.env("PATH", job.path_env)
        .arg("--ignore-config")
        .args(["--color", "never", "--no-progress"])
        .args(["--skip-download", "--write-subs", "--write-auto-subs"])
        .args(["--sub-langs", &LANGS.join(",")])
        .args(["--sub-format", "json3"])
        .args(["--ignore-errors", "--no-playlist"])
        .arg("-P")
        .arg(job.tmp)
        .args(["-o", "%(id)s.%(ext)s"]);
    if let Some(path) = job.cookies {
        cmd.arg("--cookies").arg(path);
    }
    if let Some(proxy) = job.proxy {
        cmd.args(["--proxy", proxy]);
    }
    cmd.arg("--");
    for (id, _) in videos {
        cmd.arg(format!("https://www.youtube.com/watch?v={id}"));
    }
    let _ = worker::run(ctx, cmd, |line, _| {
        // С --ignore-errors сбой загрузки субтитров приходит предупреждением.
        // Частый случай — 429 на автопереводе: YouTube ограничивает перевод субтитров.
        let msg = line
            .strip_prefix("ERROR: ")
            .or_else(|| line.strip_prefix("WARNING: "));
        if let Some(msg) = msg {
            let msg = match msg.strip_prefix("Unable to download video subtitles for ") {
                Some(rest) if rest.contains("429") => {
                    let lang = rest.split(':').next().unwrap_or(rest);
                    format!("YouTube ограничил загрузку ({lang}, HTTP 429)")
                }
                _ => msg.to_string(),
            };
            ctx.log(format!("Субтитры: {msg}"));
        }
    });
}

/// Файл субтитров видео на самом приоритетном языке из доступных.
fn pick_subtitles(tmp: &Path, id: &str) -> Option<(&'static str, PathBuf)> {
    LANGS.iter().find_map(|lang| {
        let file = tmp.join(format!("{id}.{lang}.json3"));
        file.is_file().then_some((*lang, file))
    })
}

/// Рендер субтитров в кадр. Результат — MP4 (H.264) рядом с исходником.
fn burn(ctx: &Ctx, job: &Job, video: &str, lang: &str, srt: &str) -> Result<PathBuf, String> {
    fs::write(job.tmp.join(SRT_FILE), srt).map_err(|e| e.to_string())?;
    ctx.step(format!(
        "Встраивание субтитров ({lang}): {}",
        file_name(video)
    ));
    ctx.send(worker::Event::Progress(Progress::default()));

    let target = unmarked(video, Some("mp4"));
    // Сначала во временное имя: прерванный рендер не должен оставить битый файл
    // под итоговым именем.
    let partial = unmarked(video, Some("rt-burn.mp4"));
    // Звук из MP4 — это AAC, его можно не перекодировать; opus из webm в MP4
    // поддерживают не все плееры.
    let copy_audio = Path::new(video)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4") || e.eq_ignore_ascii_case("m4a"));
    let program = job.ffmpeg_dir.join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    let mut cmd = Command::new(program);
    cmd.current_dir(job.tmp)
        .env("PATH", job.path_env)
        .args(["-y", "-hide_banner", "-nostdin", "-i"])
        .arg(video)
        .args(["-map", "0:v:0", "-map", "0:a?"])
        .args(["-vf", &format!("subtitles={SRT_FILE}")])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"])
        .args(["-pix_fmt", "yuv420p"]);
    if copy_audio {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(&partial);

    let mut duration = None;
    let mut last_error = String::new();
    let status = worker::run(ctx, cmd, |line, _| {
        if let Some(rest) = line.trim_start().strip_prefix("Duration: ") {
            duration = rest.split(',').next().and_then(parse_clock);
        } else if line.contains("time=") {
            if let Some(time) = crate::download::ffmpeg_time(line) {
                ctx.send(worker::Event::Progress(burn_progress(time, duration)));
            }
        } else {
            last_error = line.to_string();
        }
    });
    let ok = matches!(&status, Ok(s) if s.success());
    if !ok {
        let _ = fs::remove_file(&partial);
        return Err(match status {
            Err(e) => e,
            Ok(_) => last_error,
        });
    }
    fs::rename(&partial, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

fn burn_progress(time: f64, duration: Option<f64>) -> Progress {
    let written = crate::download::human_duration(time as u64);
    match duration.filter(|d| *d > 0.0) {
        Some(total) => Progress {
            ratio: Some((time / total).clamp(0.0, 1.0)),
            detail: format!(
                "{written} из {}",
                crate::download::human_duration(total.ceil() as u64)
            ),
        },
        None => Progress {
            ratio: None,
            detail: format!("обработано {written}"),
        },
    }
}

/// «00:01:02.50» → 62.5.
fn parse_clock(text: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in text.trim().split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}

/// Путь без метки `.rt-subs`; `ext` — заменить расширение.
fn unmarked(path: &str, ext: Option<&str>) -> PathBuf {
    let path = Path::new(path);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let stem = stem.unwrap_or_default();
    let stem = stem.strip_suffix(MARKER).unwrap_or(&stem);
    let ext = ext
        .map(str::to_string)
        .or_else(|| path.extension().map(|e| e.to_string_lossy().into_owned()));
    let name = match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    };
    path.with_file_name(name)
}

fn file_name(path: &str) -> String {
    unmarked(path, None)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

#[derive(Deserialize)]
struct Json3 {
    #[serde(default)]
    events: Vec<Json3Event>,
}

#[derive(Deserialize)]
struct Json3Event {
    #[serde(rename = "tStartMs", default)]
    start: i64,
    #[serde(rename = "dDurationMs", default)]
    duration: i64,
    #[serde(default)]
    segs: Vec<Json3Seg>,
}

#[derive(Deserialize)]
struct Json3Seg {
    #[serde(default)]
    utf8: String,
}

/// Субтитры YouTube в формате json3 → SRT. Для участка видео время сдвигается
/// к его началу, реплики до начала отбрасываются. `None` — реплик нет.
///
/// Автосубтитры показывают строку, пока её не вытеснит следующая через одну
/// («лесенка» из двух строк). Конец каждой реплики обрезается началом следующей,
/// чтобы на экране была одна строка.
pub fn to_srt(json: &str, section: Option<Section>) -> Option<String> {
    let data: Json3 = serde_json::from_str(json).ok()?;
    let mut cues: Vec<(i64, i64, String)> = data
        .events
        .into_iter()
        .filter_map(|event| {
            let text: String = event.segs.iter().map(|s| s.utf8.as_str()).collect();
            let text = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty() && event.duration > 0)
                .then(|| (event.start, event.start + event.duration, text))
        })
        .collect();
    cues.sort_by_key(|c| c.0);
    for i in 0..cues.len().saturating_sub(1) {
        let next = cues[i + 1].0;
        if next > cues[i].0 && next < cues[i].1 {
            cues[i].1 = next;
        }
    }

    let offset = section.map_or(0, |s| (s.start * 1000.0).round() as i64);
    let limit = section
        .and_then(|s| s.end)
        .map(|end| (end * 1000.0).round() as i64 - offset);
    let mut out = String::new();
    let mut index = 0;
    for (start, end, text) in cues {
        let (start, end) = ((start - offset).max(0), end - offset);
        if end <= 0 || limit.is_some_and(|l| start >= l) {
            continue;
        }
        index += 1;
        out.push_str(&format!(
            "{index}\n{} --> {}\n{text}\n\n",
            srt_time(start),
            srt_time(end)
        ));
    }
    (index > 0).then_some(out)
}

fn srt_time(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: &str = r#"{"events":[
        {"tStartMs":0,"dDurationMs":10000,"id":1,"wpWinPosId":1},
        {"tStartMs":1000,"dDurationMs":4000,"segs":[{"utf8":"привет"},{"utf8":" мир","tOffsetMs":500}]},
        {"tStartMs":3000,"dDurationMs":10,"aAppend":1,"segs":[{"utf8":"\n"}]},
        {"tStartMs":3000,"dDurationMs":5000,"segs":[{"utf8":"вторая строка"}]}
    ]}"#;

    #[test]
    fn converts_auto_subtitles() {
        let srt = to_srt(AUTO, None).unwrap();
        assert_eq!(
            srt,
            "1\n00:00:01,000 --> 00:00:03,000\nпривет мир\n\n\
             2\n00:00:03,000 --> 00:00:08,000\nвторая строка\n\n"
        );
    }

    #[test]
    fn shifts_to_section() {
        let section = Section {
            start: 2.0,
            end: Some(4.0),
        };
        let srt = to_srt(AUTO, Some(section)).unwrap();
        assert_eq!(
            srt,
            "1\n00:00:00,000 --> 00:00:01,000\nпривет мир\n\n\
             2\n00:00:01,000 --> 00:00:06,000\nвторая строка\n\n"
        );
        let late = Section {
            start: 60.0,
            end: None,
        };
        assert_eq!(to_srt(AUTO, Some(late)), None);
    }

    #[test]
    fn strips_marker() {
        let path = Path::new("dir").join("Видео [0m00s-0m30s].rt-subs.webm");
        let path = path.to_string_lossy();
        assert_eq!(
            unmarked(&path, None),
            Path::new("dir").join("Видео [0m00s-0m30s].webm")
        );
        assert_eq!(
            unmarked(&path, Some("mp4")),
            Path::new("dir").join("Видео [0m00s-0m30s].mp4")
        );
    }

    #[test]
    fn reads_list() {
        let dir = TempDir::create().unwrap();
        let file = dir.0.join(LIST_FILE);
        fs::write(
            &file,
            "abc D:\\v\\a b.rt-subs.mp4\nabc D:\\v\\a b.rt-subs.mp4\nxyz /v/c.webm\n",
        )
        .unwrap();
        assert_eq!(
            read_list(&file),
            vec![
                ("abc".into(), "D:\\v\\a b.rt-subs.mp4".into()),
                ("xyz".into(), "/v/c.webm".into())
            ]
        );
    }
}
