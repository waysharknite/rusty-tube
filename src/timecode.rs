//! Промежуток для загрузки части видео: разбор времени вида «2:03», «1:02:03», «95».

/// Какое из полей промежутка содержит ошибку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    From,
    To,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionError {
    pub bound: Bound,
    pub message: String,
}

impl SectionError {
    fn new(bound: Bound, message: impl Into<String>) -> Self {
        Self {
            bound,
            message: message.into(),
        }
    }
}

/// Участок видео в секундах. `end: None` — до конца видео.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Section {
    pub start: f64,
    pub end: Option<f64>,
}

/// Самый короткий участок: из более короткого ffmpeg может не собрать ни одного кадра.
const MIN_LENGTH: f64 = 1.0;

/// Символы, которые можно ввести в поле времени. Запятая — десятичный разделитель
/// в русской раскладке, приводим её к точке.
pub fn time_char(c: char) -> Option<char> {
    match c {
        '0'..='9' | ':' | '.' => Some(c),
        ',' => Some('.'),
        _ => None,
    }
}

/// Время в секундах. Пустая строка — `None` (граница не указана).
/// Форматы: `SS`, `M:SS`, `H:MM:SS`; у секунд может быть дробная часть.
pub fn parse_time(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bad_format = || format!("«{text}» — неверное время, пример: 2:03 или 1:02:03");
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return Err(bad_format());
    }
    let last = parts.len() - 1;
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value = if i == last {
            parse_seconds(part)
        } else {
            parse_integer(part)
        }
        .ok_or_else(bad_format)?;
        // Первая часть может быть любой («90:00» — полтора часа), остальные — меньше 60.
        if i > 0 && value >= 60.0 {
            return Err(format!("«{text}» — минуты и секунды должны быть меньше 60"));
        }
        total = total * 60.0 + value;
    }
    Ok(Some(total))
}

fn parse_integer(part: &str) -> Option<f64> {
    if part.is_empty() || part.len() > 9 || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse::<u32>().ok().map(f64::from)
}

fn parse_seconds(part: &str) -> Option<f64> {
    let (whole, fraction) = part.split_once('.').unwrap_or((part, ""));
    if part.contains('.') && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let whole = parse_integer(whole)?;
    let fraction = if fraction.is_empty() {
        0.0
    } else {
        format!("0.{fraction}").parse::<f64>().ok()?
    };
    Some(whole + fraction)
}

/// Проверяет поля «от» и «до». `Ok(None)` — промежуток покрывает всё видео.
pub fn parse_section(from: &str, to: &str) -> Result<Option<Section>, SectionError> {
    if from.trim().is_empty() && to.trim().is_empty() {
        return Err(SectionError::new(
            Bound::From,
            "Укажите время «от» и/или «до» или включите «Скачать видео полностью»",
        ));
    }
    let start = parse_time(from)
        .map_err(|e| SectionError::new(Bound::From, e))?
        .unwrap_or(0.0);
    let end = parse_time(to).map_err(|e| SectionError::new(Bound::To, e))?;
    if let Some(end) = end {
        if end <= start {
            return Err(SectionError::new(
                Bound::To,
                "Время «до» должно быть больше времени «от»",
            ));
        }
        if end - start < MIN_LENGTH {
            return Err(SectionError::new(
                Bound::To,
                "Промежуток должен быть не короче секунды",
            ));
        }
    }
    if start == 0.0 && end.is_none() {
        return Ok(None);
    }
    Ok(Some(Section { start, end }))
}

impl Section {
    /// Значение для `yt-dlp --download-sections`.
    pub fn spec(&self) -> String {
        let end = self.end.map_or_else(|| "inf".to_string(), seconds);
        format!("*{}-{end}", seconds(self.start))
    }

    /// Для интерфейса: «2:03–3:03», «2:03 – конец».
    pub fn label(&self) -> String {
        match self.end {
            Some(end) => format!("{}–{}", clock(self.start), clock(end)),
            None => format!("{} – конец", clock(self.start)),
        }
    }

    /// Для имени файла (без двоеточий — их нельзя в именах на Windows): «2m03s-3m03s».
    pub fn file_suffix(&self) -> String {
        let end = self.end.map_or_else(|| "end".to_string(), compact);
        format!("{}-{end}", compact(self.start))
    }

    /// Фильтр yt-dlp, отсекающий видео короче начала участка (иначе получится пустой
    /// файл или невнятная ошибка ffmpeg). `?` — пропускать видео с неизвестной длиной.
    pub fn duration_filter(&self) -> Option<String> {
        let start = self.start.floor() as u64;
        (start > 0).then(|| format!("duration>?{start}"))
    }
}

/// Секунды без лишних нулей: 123 → «123», 2.5 → «2.5».
fn seconds(t: f64) -> String {
    let text = format!("{t:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn split(t: f64) -> (u64, u64, String) {
    let whole = t.floor() as u64;
    let fraction = seconds(t - t.floor());
    // «0.5» → «.5», «0» → «».
    let fraction = fraction.trim_start_matches('0').to_string();
    (
        whole / 3600,
        whole / 60 % 60,
        format!("{:02}{fraction}", whole % 60),
    )
}

fn clock(t: f64) -> String {
    match split(t) {
        (0, m, s) => format!("{m}:{s}"),
        (h, m, s) => format!("{h}:{m:02}:{s}"),
    }
}

fn compact(t: f64) -> String {
    match split(t) {
        (0, m, s) => format!("{m}m{s}s"),
        (h, m, s) => format!("{h}h{m:02}m{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        assert_eq!(parse_time(""), Ok(None));
        assert_eq!(parse_time("  "), Ok(None));
        assert_eq!(parse_time("95"), Ok(Some(95.0)));
        assert_eq!(parse_time("2:03"), Ok(Some(123.0)));
        assert_eq!(parse_time(" 2:3 "), Ok(Some(123.0)));
        assert_eq!(parse_time("1:02:03"), Ok(Some(3723.0)));
        assert_eq!(parse_time("90:00"), Ok(Some(5400.0)));
        assert_eq!(parse_time("0:05.5"), Ok(Some(5.5)));
        assert_eq!(parse_time("00:00"), Ok(Some(0.0)));
    }

    #[test]
    fn rejects_bad_times() {
        for bad in [
            "2:",
            ":30",
            "1:2:3:4",
            "2::03",
            "abc",
            "1.5:00",
            "2:03.",
            "2:03.x",
            "-5",
            "1 2",
            "9999999999",
        ] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert!(parse_time("2:60").unwrap_err().contains("меньше 60"));
        assert!(parse_time("1:75:00").unwrap_err().contains("меньше 60"));
    }

    #[test]
    fn validates_sections() {
        let s = parse_section("2:03", "3:03").unwrap().unwrap();
        assert_eq!(
            s,
            Section {
                start: 123.0,
                end: Some(183.0)
            }
        );

        // Только «до» — с начала, только «от» — до конца.
        let s = parse_section("", "1:00").unwrap().unwrap();
        assert_eq!(
            s,
            Section {
                start: 0.0,
                end: Some(60.0)
            }
        );
        let s = parse_section("1:00", "").unwrap().unwrap();
        assert_eq!(
            s,
            Section {
                start: 60.0,
                end: None
            }
        );

        // С нуля и до конца — это всё видео.
        assert_eq!(parse_section("0:00", ""), Ok(None));

        assert_eq!(parse_section("", "").unwrap_err().bound, Bound::From);
        assert_eq!(parse_section("x", "1:00").unwrap_err().bound, Bound::From);
        assert_eq!(parse_section("1:00", "y").unwrap_err().bound, Bound::To);
        assert_eq!(parse_section("3:03", "2:03").unwrap_err().bound, Bound::To);
        assert_eq!(parse_section("2:03", "2:03").unwrap_err().bound, Bound::To);
        assert_eq!(parse_section("", "0").unwrap_err().bound, Bound::To);
        assert_eq!(parse_section("10", "10.5").unwrap_err().bound, Bound::To);
        assert!(parse_section("10", "11").is_ok());
    }

    #[test]
    fn formats_sections() {
        let s = Section {
            start: 123.0,
            end: Some(183.0),
        };
        assert_eq!(s.spec(), "*123-183");
        assert_eq!(s.label(), "2:03–3:03");
        assert_eq!(s.file_suffix(), "2m03s-3m03s");
        assert_eq!(s.duration_filter().as_deref(), Some("duration>?123"));

        let s = Section {
            start: 3725.5,
            end: None,
        };
        assert_eq!(s.spec(), "*3725.5-inf");
        assert_eq!(s.label(), "1:02:05.5 – конец");
        assert_eq!(s.file_suffix(), "1h02m05.5s-end");

        let s = Section {
            start: 0.0,
            end: Some(30.0),
        };
        assert_eq!(s.spec(), "*0-30");
        assert_eq!(s.duration_filter(), None);
    }

    #[test]
    fn filters_time_chars() {
        assert_eq!(time_char('5'), Some('5'));
        assert_eq!(time_char(','), Some('.'));
        assert_eq!(time_char('a'), None);
    }
}
