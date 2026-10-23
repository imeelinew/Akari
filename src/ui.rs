use crate::app::{App, Entry, PAGES, Regions, clean, duration};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Gauge, Paragraph, Row, Table, Tabs, Wrap},
};
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
fn panel(title: String) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Line::from(title).fg(ACCENT))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.regions = Regions::default();
    if area.width < 40 || area.height < 16 {
        frame.render_widget(
            Paragraph::new("Akari · resize to 40 × 16 · q quit").fg(ACCENT),
            area,
        );
        return;
    }
    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .split(area);
    header(frame, app, sections[0]);
    let content = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).split(sections[1]);
    let search = if app.filtering {
        format!(" / {}▏", clean(&app.query))
    } else if !app.query.is_empty() {
        format!(" / {}   · esc clear", clean(&app.query))
    } else {
        " / Search albums, artists and tracks".into()
    };
    frame.render_widget(
        Paragraph::new(search).fg(if app.filtering { ACCENT } else { MUTED }),
        content[0],
    );
    library(frame, app, content[1]);
    player(frame, app, sections[2]);
    let notice = if app.notice.is_empty() {
        " enter open / play   space pause   / search   ? help   q quit".into()
    } else {
        format!(" {}", clean(&app.notice))
    };
    frame.render_widget(
        Paragraph::new(notice).fg(if app.notice.is_empty() {
            MUTED
        } else {
            Color::Yellow
        }),
        sections[3],
    );
    if app.help {
        help(frame, area, app.help_scroll);
    }
}
fn header(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = panel(" AKARI / music ".into());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let titles = PAGES
        .iter()
        .enumerate()
        .map(|(i, p)| Line::from(format!("{} {p}", i + 1)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.page)
            .highlight_style(Style::new().fg(Color::Black).bg(ACCENT).bold())
            .style(Style::new().fg(Color::Gray))
            .divider("│")
            .padding(" ", " "),
        inner,
    );
    let mut x = inner.x;
    for (i, p) in PAGES.iter().enumerate() {
        let w = (p.len() + 4) as u16;
        app.regions.tabs.push((
            Rect::new(x, inner.y, w.min(inner.right().saturating_sub(x)), 1),
            i,
        ));
        x = x.saturating_add(w + 1);
    }
}
fn library(frame: &mut Frame, app: &mut App, area: Rect) {
    app.regions.table = area;
    let title = app.album.as_ref().map_or_else(
        || PAGES[app.page].to_owned(),
        |a| format!("{} / {}", PAGES[app.page], clean(&a.title)),
    );
    let block = panel(format!(
        " {title} · {}{} ",
        app.entries.len(),
        if app.loading { " · loading" } else { "" }
    ));
    if app.entries.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let text = if app.loading {
            "Loading library…"
        } else if !app.query.is_empty() {
            "No matches · esc to clear search"
        } else if app.page == 2 {
            "Queue is empty · a to add a track"
        } else if app.page == 3 {
            "No transfers · d to download a track or album"
        } else if app.album.is_some() {
            "No tracks"
        } else if app.page == 1 {
            "No downloaded albums"
        } else {
            "No albums · configure a server or external library"
        };
        frame.render_widget(
            Paragraph::new(text)
                .fg(MUTED)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            Rect::new(
                inner.x,
                inner.y + inner.height / 2,
                inner.width,
                inner.height.saturating_sub(inner.height / 2),
            ),
        );
        return;
    }
    let album_rows = matches!(app.entries.first(), Some(Entry::Album(_)));
    let rows = app.entries.iter().enumerate().map(|(i, e)| {
        let (number, last, status) = match e {
            Entry::Album(a) => (
                format!("{:02}", i + 1),
                if a.year > 0 {
                    a.year.to_string()
                } else {
                    a.genre.clone()
                },
                String::new(),
            ),
            Entry::Song {
                song,
                index,
                status,
            } => (
                if app.page == 2 {
                    format!("{:02}", index + 1)
                } else if song.track_number > 0 {
                    format!("{:02}", song.track_number)
                } else {
                    format!("{:02}", i + 1)
                },
                duration(song.duration),
                status.clone(),
            ),
        };
        let current = e
            .song()
            .is_some_and(|s| app.state.current().is_some_and(|c| c.id == s.id))
            && (app.page != 2
                || matches!(e,Entry::Song{index,..} if *index as i32==app.state.index));
        Row::new(vec![
            number,
            clean(e.title()),
            clean(e.artist()),
            clean(&last),
            clean(&status),
        ])
        .style(if current {
            Style::new().fg(ACCENT)
        } else {
            Style::new()
        })
    });
    let widths = [
        Constraint::Length(3),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(if album_rows { 9 } else { 6 }),
        Constraint::Length(if app.page == 3 { 22 } else { 8 }),
    ];
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .block(block)
            .header(
                Row::new([
                    "#",
                    if album_rows { "Album" } else { "Track" },
                    "Artist",
                    if album_rows { "Year" } else { "Time" },
                    "Status",
                ])
                .style(Style::new().fg(MUTED)),
            )
            .column_spacing(1)
            .row_highlight_style(Style::new().bg(Color::DarkGray).fg(Color::White).bold())
            .highlight_symbol("› "),
        area,
        &mut app.table,
    );
}
fn player(frame: &mut Frame, app: &mut App, area: Rect) {
    let mode = if app.buffering {
        "BUFFERING"
    } else if app.paused {
        "PAUSED"
    } else {
        "PLAYING"
    };
    let block = panel(format!(" {mode} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let track = app.state.current().map_or_else(
        || "Nothing playing · choose an album to begin".into(),
        |s| format!("{}  ·  {}", clean(&s.title), clean(&s.artist)),
    );
    frame.render_widget(Paragraph::new(track).bold(), rows[0]);
    let progress = Layout::horizontal([
        Constraint::Length(7),
        Constraint::Min(1),
        Constraint::Length(8),
    ])
    .split(rows[1]);
    frame.render_widget(
        Paragraph::new(duration(app.state.position)).fg(MUTED),
        progress[0],
    );
    let ratio = if app.duration > 0. && app.duration.is_finite() {
        (app.state.position / app.duration).clamp(0., 1.)
    } else {
        0.
    };
    frame.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label("")
            .gauge_style(Style::new().fg(ACCENT).bg(Color::DarkGray))
            .use_unicode(true),
        progress[1],
    );
    app.regions.seek = progress[1];
    frame.render_widget(
        Paragraph::new(duration(app.duration))
            .alignment(Alignment::Right)
            .fg(MUTED),
        progress[2],
    );
    let labels = [
        (
            if app.paused { "▶ Play" } else { "Ⅱ Pause" },
            KeyCode::Char(' '),
        ),
        ("‹ Prev", KeyCode::Char('p')),
        ("Next ›", KeyCode::Char('n')),
        (
            if app.state.shuffle {
                "Shuffle on"
            } else {
                "Shuffle off"
            },
            KeyCode::Char('s'),
        ),
        (
            ["Repeat off", "Repeat all", "Repeat one"][app.state.repeat as usize],
            KeyCode::Char('r'),
        ),
        (
            if app.spatial {
                "Spatial auto"
            } else {
                "Spatial off"
            },
            KeyCode::Char('o'),
        ),
    ];
    let mut spans = vec![];
    let mut x = rows[2].x;
    for (label, key) in labels {
        let text = format!("[{label}] ");
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        app.regions.controls.push((
            Rect::new(
                x,
                rows[2].y,
                w.saturating_sub(1).min(rows[2].right().saturating_sub(x)),
                1,
            ),
            key,
        ));
        x = x.saturating_add(w);
        spans.push(Span::styled(text, Style::new().fg(Color::Gray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[2]);
    let volume = format!("[-] Vol {:3.0}% [+]", app.state.volume);
    frame.render_widget(Paragraph::new(volume).fg(ACCENT), rows[3]);
    app.regions.volume = Rect::new(rows[3].x, rows[3].y, 18.min(rows[3].width), 1);
    app.regions
        .controls
        .push((Rect::new(rows[3].x, rows[3].y, 3, 1), KeyCode::Char('-')));
    app.regions.controls.push((
        Rect::new(rows[3].x + 13, rows[3].y, 3, 1),
        KeyCode::Char('+'),
    ));
}
fn help(frame: &mut Frame, area: Rect, scroll: u16) {
    let w = area.width.saturating_sub(4).min(76);
    let h = area.height.saturating_sub(2).min(22);
    let popup = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(" Browse").fg(ACCENT).bold(),
        Line::from(" ↑ ↓ / j k    Move           enter       Open / play"),
        Line::from(" 1–4 / tab    Switch view    esc         Back / clear"),
        Line::from(" /            Search         R           Refresh"),
        Line::from(""),
        Line::from(" Playback").fg(ACCENT).bold(),
        Line::from(" space        Play / pause   p / n       Previous / next"),
        Line::from(" ← → / h l    Seek 5 sec     + / -       Volume"),
        Line::from(" s / r        Shuffle / repeat           o   Spatial"),
        Line::from(""),
        Line::from(" Queue & downloads").fg(ACCENT).bold(),
        Line::from(" a / N        Queue / next   J / K       Reorder queue"),
        Line::from(" d / x        Download / remove (x twice confirms)"),
        Line::from(" c            Cancel / trim upcoming     C   Stop & clear"),
        Line::from(""),
        Line::from(" Mouse: select, double-click, wheel, seek, controls"),
        Line::from(" ? / esc      Close help     q / ctrl-c  Quit"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(panel(" Help · ↑↓ scroll · esc close ".into()))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn help_renders_at_minimum_and_normal_sizes() {
        for (w, h) in [(40, 16), (100, 32)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| help(f, f.area(), 0)).unwrap();
            let text = t
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("Help ·"));
        }
    }
}
