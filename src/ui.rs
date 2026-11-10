//! Отрисовка интерфейса.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap},
};

use crate::{
    app::{App, Confirm, DownloadView, Focus, Modal, Mode, SearchView, SettingsForm, Stage, field},
    config::{AudioFormat, Browser, Quality, Theme},
    download::Kind,
    input::Input,
    theme::Palette,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits = Default::default();
    // Пока открыты настройки, рисуем выбранной там темой — это предпросмотр.
    let theme = match &app.modal {
        Some(Modal::Settings(form)) => Theme::ALL[form.theme],
        _ => app.config.theme,
    };
    let p = &theme.palette();
    frame.render_widget(Block::new().style(p.base), frame.area());
    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_top_bar(frame, app, top, p);
    match &app.stage {
        Stage::Startup => draw_startup(frame, app, body, p),
        Stage::Ready => draw_main(frame, app, body, p),
        Stage::Fatal(message) => {
            let message = message.clone();
            draw_fatal(frame, &message, body, p);
        }
    }
    draw_status_bar(frame, app, bottom, p);

    let modal = app.modal.take();
    match &modal {
        Some(Modal::Confirm(confirm)) => draw_confirm(frame, app, confirm, p),
        Some(Modal::Settings(form)) => draw_settings(frame, app, form, p),
        None => {}
    }
    app.modal = modal;
}

fn spinner(app: &App) -> &'static str {
    SPINNER[app.tick % SPINNER.len()]
}

fn draw_top_bar(frame: &mut Frame, app: &mut App, area: Rect, p: &Palette) {
    frame.render_widget(Block::new().style(Style::new().fg(p.text).bg(p.bar)), area);
    let title = Line::from(vec![
        Span::styled(" ▶ ", Style::new().fg(p.accent).bold()),
        Span::styled("rusty-tube", Style::new().bold()),
    ]);
    frame.render_widget(Paragraph::new(title), area);

    let ready = matches!(app.stage, Stage::Ready) && app.modal.is_none();

    // Вкладки режимов: активная подсвечена, переключение — F3 или клик.
    let mut x = area.x + 16;
    for (mode, label) in [(Mode::Video, " Видео "), (Mode::Music, " Музыка ")] {
        let w = text_width(label);
        let rect = Rect::new(x, area.y, w, 1).intersection(area);
        let style = if app.mode == mode {
            Style::new()
                .fg(p.on_accent)
                .bg(if ready { p.accent } else { p.dim })
                .bold()
        } else {
            Style::new()
                .fg(if ready { p.text } else { p.dim })
                .bg(p.bar)
        };
        frame.render_widget(Paragraph::new(label).style(style), rect);
        app.hits.modes.push((rect, mode));
        x += w + 1;
    }
    let hint = Rect::new(x, area.y, 4, 1).intersection(area);
    frame.render_widget(Paragraph::new("F3").fg(p.dim), hint);

    let settings_label = " Настройки F2 ";
    let exit_label = " Выход F10 ";
    let exit_w = text_width(exit_label);
    let settings_w = text_width(settings_label);
    let exit = Rect::new(area.right().saturating_sub(exit_w + 1), area.y, exit_w, 1);
    let settings = Rect::new(exit.x.saturating_sub(settings_w + 1), area.y, settings_w, 1);

    let button_style = |focused: bool, enabled: bool| {
        if !enabled {
            Style::new().fg(p.dim).bg(p.bar)
        } else if focused {
            Style::new().fg(p.on_accent).bg(p.accent).bold()
        } else {
            Style::new().fg(p.on_control).bg(p.control)
        }
    };
    frame.render_widget(
        Paragraph::new(settings_label)
            .style(button_style(ready && app.focus == Focus::Settings, ready)),
        settings,
    );
    frame.render_widget(
        Paragraph::new(exit_label).style(button_style(ready && app.focus == Focus::Exit, true)),
        exit,
    );
    app.hits.settings = settings;
    app.hits.exit = exit;
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect, p: &Palette) {
    let sep = Span::styled("  │  ", Style::new().fg(p.dim));
    let (format_label, format, dir) = match app.mode {
        Mode::Video => (
            " Качество: ",
            app.config.quality.label(),
            &app.config.download_dir,
        ),
        Mode::Music => (
            " Формат: ",
            app.config.audio_format.label(),
            &app.config.music_dir,
        ),
    };
    let line = Line::from(vec![
        Span::styled(format_label, Style::new().fg(p.dim)),
        Span::raw(format),
        sep.clone(),
        Span::styled("Cookie: ", Style::new().fg(p.dim)),
        Span::raw(app.config.browser.label()),
        sep.clone(),
        Span::styled("Прокси: ", Style::new().fg(p.dim)),
        Span::raw(
            app.config
                .proxy_port
                .map_or_else(|| "нет".to_string(), |p| format!("SOCKS5 :{p}")),
        ),
        sep,
        Span::styled("Папка: ", Style::new().fg(p.dim)),
        Span::raw(dir.to_string_lossy().into_owned()),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::new().fg(p.text).bg(p.bar)),
        area,
    );
}

/// Центральная колонка фиксированной максимальной ширины.
fn column(area: Rect, max_width: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(max_width);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y,
        width,
        area.height,
    )
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn text_width(text: &str) -> u16 {
    text.chars().count() as u16
}

fn panel(title: &str, color: Color) -> Block<'_> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(format!(" {title} ")).bold())
}

// ------------------------------------------------------------------ экраны

fn draw_startup(frame: &mut Frame, app: &App, area: Rect, p: &Palette) {
    let rect = centered(area, 84, 18);
    let block = panel(&app.startup.title, p.accent);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let [step, _, log, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner.inner(ratatui::layout::Margin::new(1, 0)));

    let busy = app.busy();
    let marker = if busy { spinner(app) } else { "•" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{marker} "), Style::new().fg(p.accent)),
            Span::raw(app.startup.step.as_str()),
        ])),
        step,
    );

    let lines: Vec<Line> = app
        .startup
        .log
        .iter()
        .rev()
        .take(log.height as usize)
        .rev()
        .map(|l| Line::styled(l.as_str(), Style::new().fg(p.dim)))
        .collect();
    frame.render_widget(Paragraph::new(lines), log);

    let hint_text = if busy {
        "Esc — прервать шаг · F10 — выход"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(hint_text)
            .fg(p.dim)
            .alignment(Alignment::Center),
        hint,
    );
}

fn draw_fatal(frame: &mut Frame, message: &str, area: Rect, p: &Palette) {
    let rect = centered(area, 72, 12);
    let block = panel("Ошибка", p.err);
    let inner = block.inner(rect).inner(ratatui::layout::Margin::new(1, 1));
    frame.render_widget(block, rect);
    let [text, hint] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), text);
    frame.render_widget(
        Paragraph::new("Нажмите любую клавишу для выхода")
            .fg(p.dim)
            .alignment(Alignment::Center),
        hint,
    );
}

fn draw_main(frame: &mut Frame, app: &mut App, area: Rect, p: &Palette) {
    let col = column(area, 96);
    let has_search = app.mode == Mode::Music && app.search.is_some();
    let panel_height = match (app.download.is_some(), has_search) {
        (false, _) => 0,
        // Рядом со списком результатов панель загрузки поменьше.
        (true, true) => 11.min(col.height.saturating_sub(14)),
        (true, false) => 16.min(col.height.saturating_sub(7)),
    };
    // Со списком результатов всё прижимается к верху, иначе — по центру.
    let (top, results_height, bottom) = if has_search {
        (
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(0),
        )
    } else {
        (
            Constraint::Fill(1),
            Constraint::Length(0),
            Constraint::Fill(1),
        )
    };
    let [_, label, input, hint, _, results_area, _, panel_area, _] = Layout::vertical([
        top,
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        results_height,
        Constraint::Length(if has_search && panel_height > 0 { 1 } else { 0 }),
        Constraint::Length(panel_height),
        bottom,
    ])
    .areas(col);

    let (title, placeholder) = match app.mode {
        Mode::Video => (
            "Ссылка на видео или плейлист YouTube",
            "https://www.youtube.com/watch?v=…  (Ctrl+V — вставить)",
        ),
        Mode::Music => (
            "Поиск в YouTube Music",
            "Название песни, исполнитель или ссылка на трек/альбом",
        ),
    };
    frame.render_widget(
        Paragraph::new(title).bold().alignment(Alignment::Center),
        label,
    );

    let focused = app.focus == Focus::Input && app.modal.is_none();
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { p.accent } else { p.dim }));
    let inner = block.inner(input);
    frame.render_widget(block, input);
    app.hits.input = input;

    let text_area = inner.inner(ratatui::layout::Margin::new(1, 0));
    let field = app.current_input();
    if field.is_empty() {
        frame.render_widget(Paragraph::new(placeholder).fg(p.dim), text_area);
    } else {
        let (visible, cursor) = field.visible(text_area.width as usize);
        frame.render_widget(Paragraph::new(visible), text_area);
        if focused {
            frame.set_cursor_position((text_area.x + cursor as u16, text_area.y));
        }
    }
    if focused && field.is_empty() {
        frame.set_cursor_position((text_area.x, text_area.y));
    }

    let hint_line = match &app.notice {
        Some(notice) => Line::styled(notice.as_str(), Style::new().fg(p.warn)),
        None if app.downloading() => {
            Line::styled("Esc — прервать загрузку", Style::new().fg(p.dim))
        }
        None if app.searching() => Line::styled("Esc — остановить поиск", Style::new().fg(p.dim)),
        None if app.mode == Mode::Music && app.has_results() => Line::styled(
            "Enter — найти · ↓ — к результатам · Esc — очистить",
            Style::new().fg(p.dim),
        ),
        None if app.mode == Mode::Music => Line::styled(
            "Enter — найти или скачать по ссылке · F3 — видео",
            Style::new().fg(p.dim),
        ),
        None => Line::styled(
            "Enter — скачать · Esc — очистить · F3 — музыка",
            Style::new().fg(p.dim),
        ),
    };
    frame.render_widget(Paragraph::new(hint_line).alignment(Alignment::Center), hint);

    if has_search {
        let tick = app.tick;
        let focused = app.focus == Focus::Results && app.modal.is_none();
        let searching = app.searching();
        if let Some(search) = app.search.as_ref() {
            let (list, rows) = draw_results(
                frame,
                search,
                results_area,
                focused,
                searching.then(|| SPINNER[tick % SPINNER.len()]),
                p,
            );
            app.hits.results = list;
            app.hits.result_rows = rows;
        }
    }

    if panel_height > 0 {
        let tick = app.tick;
        if let Some(view) = app.download.as_ref() {
            let open = draw_download(frame, view, panel_area, SPINNER[tick % SPINNER.len()], p);
            app.hits.open_folder = open;
        }
    }
}

/// Список найденных треков. Возвращает область списка и строки для кликов.
fn draw_results(
    frame: &mut Frame,
    search: &SearchView,
    area: Rect,
    focused: bool,
    spin: Option<&str>,
    p: &Palette,
) -> (Rect, Vec<(Rect, usize)>) {
    let title = match spin {
        Some(spin) => format!(
            "{spin} Поиск: {} · найдено {}",
            search.query,
            search.results.len()
        ),
        None => format!("Результаты: {}", search.query),
    };
    let block = panel(&title, if focused { p.accent } else { p.dim });
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, area);
    if inner.height == 0 {
        return (Rect::default(), Vec::new());
    }

    let [list, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    if search.results.is_empty() {
        let text = match (&search.error, spin) {
            (Some(err), _) => Line::styled(err.as_str(), Style::new().fg(p.err)),
            (None, Some(_)) => Line::styled(
                "Каждый результат загружается около 2 секунд…",
                Style::new().fg(p.dim),
            ),
            (None, None) => Line::styled("Ничего не найдено", Style::new().fg(p.dim)),
        };
        frame.render_widget(Paragraph::new(text), list);
        return (list, Vec::new());
    }

    // Прокрутка: выбранная строка всегда видна.
    let height = list.height as usize;
    let offset = search.selected.saturating_sub(height.saturating_sub(1));
    let width = list.width as usize;
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for (i, track) in search.results.iter().enumerate().skip(offset).take(height) {
        let selected = i == search.selected;
        let duration = track.duration();
        let album = track.album.as_deref().unwrap_or("");
        // Слева «Исполнитель — Трек», справа альбом и длительность.
        let right = if album.is_empty() || album == track.name() {
            format!("  {duration:>6}")
        } else {
            format!("  {}  {duration:>6}", truncate(album, width / 3))
        };
        let left_width = width.saturating_sub(right.chars().count() + 2);
        let artist = track.artist();
        let name = track.name();
        let left = if artist.is_empty() {
            truncate(name, left_width)
        } else {
            truncate(&format!("{artist} — {name}"), left_width)
        };
        let pad = width.saturating_sub(left.chars().count() + right.chars().count() + 2);
        let marker = if selected { "▶ " } else { "  " };
        let base = if selected && focused {
            Style::new().fg(p.on_accent).bg(p.accent)
        } else if selected {
            Style::new().fg(p.on_control).bg(p.control)
        } else {
            Style::new()
        };
        let dim = if selected { base } else { base.fg(p.dim) };
        lines.push(Line::from(vec![
            Span::styled(marker, base.bold()),
            Span::styled(left, base.bold()),
            Span::styled(" ".repeat(pad), base),
            Span::styled(right, dim),
        ]));
        let y = list.y + (i - offset) as u16;
        rows.push((Rect::new(list.x, y, list.width, 1), i));
    }
    frame.render_widget(Paragraph::new(lines), list);

    let footer_text = match (&search.error, focused) {
        (Some(err), _) => Line::styled(err.as_str(), Style::new().fg(p.warn)),
        (None, true) => Line::styled(
            "↑↓ — выбор · Enter — скачать · Esc — к поиску · Ctrl+O — папка",
            Style::new().fg(p.dim),
        ),
        (None, false) => Line::styled(
            "↓ или Tab — к списку · клик — выбрать, повторный клик — скачать",
            Style::new().fg(p.dim),
        ),
    };
    frame.render_widget(Paragraph::new(footer_text), footer);
    (list, rows)
}

/// Обрезает строку до `max` символов с многоточием.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Возвращает область кнопки «Открыть папку» (если показана).
fn draw_download(
    frame: &mut Frame,
    view: &DownloadView,
    area: Rect,
    spin: &str,
    p: &Palette,
) -> Rect {
    let (title, color) = match &view.result {
        None if view.playlist => ("Загрузка плейлиста", p.accent),
        None if view.kind == Kind::Audio => ("Загрузка трека", p.accent),
        None => ("Загрузка", p.accent),
        Some(Ok(_)) => ("Готово", p.ok),
        Some(Err(_)) => ("Ошибка", p.err),
    };
    let block = panel(title, color);
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, area);

    let has_items = view.item.is_some();
    let [url, steps, gauge, detail, items, result] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(if has_items { 1 } else { 0 }),
        Constraint::Length(2),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(view.label.as_str()).fg(p.dim), url);

    // История шагов: выполненные — галочкой, текущий — спиннером.
    let running = view.running();
    let failed = matches!(view.result, Some(Err(_)));
    let step_lines: Vec<Line> = view
        .steps
        .iter()
        .rev()
        .take(steps.height as usize)
        .rev()
        .map(|step| {
            let (mark, style) = if step.done {
                ("✓", Style::new().fg(p.ok))
            } else if running {
                (spin, Style::new().fg(p.accent).bold())
            } else if failed {
                ("✗", Style::new().fg(p.err))
            } else {
                ("•", Style::new())
            };
            let text_style = if step.done {
                Style::new().fg(p.dim)
            } else {
                Style::new().bold()
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(step.text.as_str(), text_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(step_lines), steps);

    let current = view.steps.last().map_or("Подготовка", |s| s.text.as_str());
    let (ratio, label) = match (view.progress.ratio, &view.result) {
        (_, Some(Ok(_))) => (1.0, "100%".to_string()),
        (Some(r), _) => (r, format!("{:.1}%", r * 100.0)),
        (None, _) => (0.0, if running { "…".into() } else { String::new() }),
    };
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(color).bg(p.bar))
            .ratio(ratio)
            .label(Span::styled(
                label,
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            ))
            .use_unicode(true),
        gauge,
    );
    let detail_text = if running && !view.progress.detail.is_empty() {
        view.progress.detail.clone()
    } else if running {
        current.to_string()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(detail_text).fg(p.dim), detail);

    if let Some((index, total)) = view.item {
        let done = if running {
            index.saturating_sub(1)
        } else {
            total
        };
        let ratio = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let [label, bar] =
            Layout::horizontal([Constraint::Length(22), Constraint::Fill(1)]).areas(items);
        frame.render_widget(
            Paragraph::new(format!("Плейлист: {index} из {total}")),
            label,
        );
        frame.render_widget(
            Gauge::default()
                .gauge_style(Style::new().fg(p.info).bg(p.bar))
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{done}/{total}"))
                .use_unicode(true),
            bar,
        );
    }

    let mut open_rect = Rect::default();
    let lines: Vec<Line> = match &view.result {
        None => view
            .log
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|l| Line::styled(l.as_str(), Style::new().fg(p.warn)))
            .collect(),
        Some(Ok(summary)) => {
            let mut text = format!("Сохранено файлов: {}", summary.files.len());
            if !summary.errors.is_empty() {
                text.push_str(&format!(" · с ошибками: {}", summary.errors.len()));
            }
            let button = " Открыть папку Ctrl+O ";
            open_rect = Rect::new(
                result.x + text_width(&text) + 2,
                result.y,
                text_width(button).min(result.width.saturating_sub(text_width(&text) + 2)),
                1,
            );
            vec![
                Line::from(vec![
                    Span::styled(text, Style::new().fg(p.ok).bold()),
                    Span::raw("  "),
                    Span::styled(button, Style::new().fg(p.on_accent).bg(p.ok)),
                ]),
                Line::styled(
                    summary.errors.last().map_or(
                        match view.kind {
                            Kind::Video => "Можно вставить следующую ссылку",
                            Kind::Audio => "Можно выбрать следующий трек",
                        },
                        String::as_str,
                    ),
                    Style::new().fg(if summary.errors.is_empty() {
                        p.dim
                    } else {
                        p.warn
                    }),
                ),
            ]
        }
        Some(Err(err)) => vec![Line::styled(err.as_str(), Style::new().fg(p.err))],
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), result);
    open_rect
}

// ------------------------------------------------------------------ модальные окна

fn button(frame: &mut Frame, area: Rect, label: &str, focused: bool, p: &Palette) {
    let style = if focused {
        Style::new().fg(p.on_accent).bg(p.accent).bold()
    } else {
        Style::new().fg(p.on_control).bg(p.control)
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(style)
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_confirm(frame: &mut Frame, app: &mut App, confirm: &Confirm, p: &Palette) {
    let width = 66u16;
    let inner_width = width.saturating_sub(4) as usize;
    let body_height: usize = confirm
        .body
        .iter()
        .map(|l| (l.chars().count().max(1)).div_ceil(inner_width))
        .sum();
    let rect = centered(frame.area(), width, body_height as u16 + 6);
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::new().style(p.base), rect);
    let block = panel(&confirm.title, p.accent);
    let inner = block.inner(rect).inner(ratatui::layout::Margin::new(1, 1));
    frame.render_widget(block, rect);

    let [body, buttons] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    let lines: Vec<Line> = confirm.body.iter().map(|l| Line::raw(l.as_str())).collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);

    let yes = format!(" {} ", confirm.yes);
    let no = format!(" {} ", confirm.no);
    let (yes_w, no_w) = (text_width(&yes) + 2, text_width(&no) + 2);
    let total = yes_w + 2 + no_w;
    let x = buttons.x + buttons.width.saturating_sub(total) / 2;
    let yes_rect = Rect::new(x, buttons.y, yes_w, 1);
    let no_rect = Rect::new(x + yes_w + 2, buttons.y, no_w, 1);
    button(frame, yes_rect, &yes, confirm.yes_selected, p);
    button(frame, no_rect, &no, !confirm.yes_selected, p);
    app.hits.modal = vec![(yes_rect, 0), (no_rect, 1)];
}

fn draw_settings(frame: &mut Frame, app: &mut App, form: &SettingsForm, p: &Palette) {
    let rect = centered(frame.area(), 78, 28);
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::new().style(p.base), rect);
    let title = if form.first_run {
        "Первоначальная настройка"
    } else {
        "Настройки"
    };
    let block = panel(title, p.accent);
    let inner = block.inner(rect).inner(ratatui::layout::Margin::new(2, 1));
    frame.render_widget(block, rect);

    let [
        quality,
        audio,
        theme,
        _,
        browser,
        browser_hint,
        _,
        proxy,
        proxy_hint,
        _,
        folder_label,
        folder,
        music_label,
        music_folder,
        error,
        buttons,
        _,
        help,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut hits = Vec::new();

    selector(
        frame,
        quality,
        "Качество видео",
        Quality::ALL[form.quality].label(),
        form.focus == field::QUALITY,
        p,
    );
    hits.push((quality, field::QUALITY));

    selector(
        frame,
        audio,
        "Формат музыки",
        AudioFormat::ALL[form.audio].label(),
        form.focus == field::AUDIO,
        p,
    );
    hits.push((audio, field::AUDIO));

    selector(
        frame,
        theme,
        "Тема оформления",
        Theme::ALL[form.theme].label(),
        form.focus == field::THEME,
        p,
    );
    hits.push((theme, field::THEME));

    let chosen = Browser::ALL[form.browser];
    selector(
        frame,
        browser,
        "Cookie из браузера",
        chosen.label(),
        form.focus == field::BROWSER,
        p,
    );
    hits.push((browser, field::BROWSER));
    frame.render_widget(
        Paragraph::new(format!("{:22}{}", "", chosen.location_hint())).fg(p.dim),
        browser_hint,
    );

    // Поле порта: подпись как у селекторов, значение — подсвеченная строка ввода.
    let proxy_focused = form.focus == field::PROXY;
    frame.render_widget(
        Paragraph::new(format!("{:22}", "SOCKS5 прокси (порт)")),
        proxy,
    );
    let proxy_field = Rect::new(
        proxy.x + 22,
        proxy.y,
        24.min(proxy.width.saturating_sub(22)),
        1,
    );
    let field_style = if proxy_focused {
        Style::new().fg(p.on_accent).bg(p.accent)
    } else {
        Style::new().fg(p.on_control).bg(p.control)
    };
    frame.render_widget(Paragraph::new("").style(field_style), proxy_field);
    let proxy_text = proxy_field.inner(ratatui::layout::Margin::new(1, 0));
    let (visible, cursor) = form.proxy.visible(proxy_text.width as usize);
    frame.render_widget(Paragraph::new(visible).style(field_style), proxy_text);
    if proxy_focused {
        frame.set_cursor_position((proxy_text.x + cursor as u16, proxy_text.y));
    }
    hits.push((proxy_field, field::PROXY));
    frame.render_widget(
        Paragraph::new(format!(
            "{:22}127.0.0.1:<порт> из VPN-клиента · пусто — без прокси",
            ""
        ))
        .fg(p.dim),
        proxy_hint,
    );

    folder_field(
        frame,
        form,
        &mut hits,
        (folder_label, folder),
        ("Папка для видео", &form.folder),
        (field::FOLDER, field::BROWSE),
        p,
    );
    folder_field(
        frame,
        form,
        &mut hits,
        (music_label, music_folder),
        ("Папка для музыки", &form.music_folder),
        (field::MUSIC_FOLDER, field::MUSIC_BROWSE),
        p,
    );

    if let Some(err) = &form.error {
        frame.render_widget(Paragraph::new(err.as_str()).fg(p.err), error);
    }

    let save_w = 14;
    let cancel_w = if form.first_run { 0 } else { 12 };
    let total = save_w + if form.first_run { 0 } else { cancel_w + 2 };
    let x = buttons.right().saturating_sub(total);
    let save_rect = Rect::new(x, buttons.y, save_w, 1);
    button(frame, save_rect, "Сохранить", form.focus == field::SAVE, p);
    hits.push((save_rect, field::SAVE));
    if !form.first_run {
        let cancel_rect = Rect::new(x + save_w + 2, buttons.y, cancel_w, 1);
        button(frame, cancel_rect, "Отмена", form.focus == field::CANCEL, p);
        hits.push((cancel_rect, field::CANCEL));
    }

    let help_text = if form.first_run {
        "Tab/↑↓ — поле · ←→ — значение · Enter — выбрать"
    } else {
        "Tab/↑↓ — поле · ←→ — значение · Enter — выбрать · Esc — отмена"
    };
    frame.render_widget(
        Paragraph::new(help_text)
            .fg(p.dim)
            .alignment(Alignment::Center),
        help,
    );
    app.hits.modal = hits;
}

/// Подпись с кнопкой «Обзор…» справа и поле с путём под ней.
fn folder_field(
    frame: &mut Frame,
    form: &SettingsForm,
    hits: &mut Vec<(Rect, usize)>,
    (label, area): (Rect, Rect),
    (title, input): (&str, &Input),
    (text_field, browse_field): (usize, usize),
    p: &Palette,
) {
    frame.render_widget(Paragraph::new(title), label);
    let browse_rect = Rect::new(label.right().saturating_sub(16), label.y, 16, 1);
    button(frame, browse_rect, "Обзор…", form.focus == browse_field, p);
    hits.push((browse_rect, browse_field));

    let focused = form.focus == text_field;
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { p.accent } else { p.dim }));
    let text_area = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, area);
    let (visible, cursor) = input.visible(text_area.width as usize);
    frame.render_widget(Paragraph::new(visible), text_area);
    if focused {
        frame.set_cursor_position((text_area.x + cursor as u16, text_area.y));
    }
    hits.push((area, text_field));
}

fn selector(frame: &mut Frame, area: Rect, label: &str, value: &str, focused: bool, p: &Palette) {
    let value_style = if focused {
        Style::new().fg(p.on_accent).bg(p.accent).bold()
    } else {
        Style::new().bold()
    };
    let arrow_style = Style::new().fg(if focused { p.accent } else { p.dim });
    let line = Line::from(vec![
        Span::raw(format!("{label:22}")),
        Span::styled("◀ ", arrow_style),
        Span::styled(format!(" {value:^18} "), value_style),
        Span::styled(" ▶", arrow_style),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
