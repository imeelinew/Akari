use crate::app::{App, Entry, PAGES, Regions, clean, duration};
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Clear, Gauge, HighlightSpacing, Paragraph, Row, Table, TableState, Tabs,
        Wrap,
    },
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
    let mut cache = std::mem::take(&mut app.ui_cache);
    let body = sections[1];
    let (left, right) = if body.width >= 76 {
        let panes = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(body);
        (panes[0], panes[1])
    } else {
        (body, Rect::default())
    };
    let content = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).split(left);
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
    app.regions.table = content[1];
    if !cache.library.restore(frame, content[1], app.view_revision) {
        library(frame, app, content[1]);
        cache.library.store(frame, content[1], app.view_revision);
    }
    if right.width > 0 {
        let panes =
            Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)]).split(right);
        cover(frame, app, panes[0]);
        pulse(frame, app, panes[1], &mut cache.spectrum);
    }
    // The footer and all formatted library rows stay cached between FFT frames.
    let footer_revision = app
        .playback_revision
        .wrapping_add(app.visual.revision.wrapping_mul(31));
    if cache.footer.restore(frame, sections[2], footer_revision) {
        app.regions.seek = cache.seek;
        app.regions.volume = cache.volume;
        app.regions.controls = cache.controls.clone();
    } else {
        player(frame, app, sections[2]);
        cache.footer.store(frame, sections[2], footer_revision);
        cache.seek = app.regions.seek;
        cache.volume = app.regions.volume;
        cache.controls = app.regions.controls.clone();
    }
    app.ui_cache = cache;
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
#[derive(Default)]
pub struct Cache {
    library: CachedRegion,
    footer: CachedRegion,
    seek: Rect,
    volume: Rect,
    controls: Vec<(Rect, KeyCode)>,
    spectrum: SpectrumCache,
}
#[derive(Default)]
struct CachedRegion {
    revision: u64,
    buffer: Option<Buffer>,
}
impl CachedRegion {
    fn restore(&self, frame: &mut Frame, area: Rect, revision: u64) -> bool {
        let Some(buffer) = &self.buffer else {
            return false;
        };
        if buffer.area != area || self.revision != revision {
            return false;
        }
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.buffer_mut()[(x, y)] = buffer[(x, y)].clone();
            }
        }
        true
    }
    fn store(&mut self, frame: &mut Frame, area: Rect, revision: u64) {
        let mut buffer = self
            .buffer
            .take()
            .filter(|b| b.area == area)
            .unwrap_or_else(|| Buffer::empty(area));
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)] = frame.buffer_mut()[(x, y)].clone();
            }
        }
        self.buffer = Some(buffer);
        self.revision = revision;
    }
}
#[derive(Default)]
struct SpectrumCache {
    palette: Option<crate::artwork::Palette>,
    colors: Vec<Color>,
}
fn cover(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = panel(" COVER ".into());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let parts = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(inner);
    app.regions.cover = parts[0];
    let slot = &app.visual.cover;
    let metadata = slot.album.as_ref().map_or_else(
        || vec![Line::from("Choose an album").fg(MUTED)],
        |a| {
            vec![
                Line::from(clean(&a.title)).bold(),
                Line::from(clean(&a.artist)).fg(MUTED),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(metadata).alignment(Alignment::Center),
        parts[1],
    );
    let message = if slot.loading {
        "Loading cover…"
    } else if slot.image.is_none() {
        "No cover"
    } else if !crate::artwork::Graphics::supported() {
        "Kitty graphics unavailable"
    } else {
        ""
    };
    if !message.is_empty() {
        frame.render_widget(
            Paragraph::new(message)
                .fg(MUTED)
                .alignment(Alignment::Center),
            Rect::new(
                parts[0].x,
                parts[0].y + parts[0].height / 2,
                parts[0].width,
                u16::from(parts[0].height > 0),
            ),
        );
    }
}
fn pulse(frame: &mut Frame, app: &mut App, area: Rect, cache: &mut SpectrumCache) {
    let block = panel(" PULSE ".into());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.regions.spectrum = inner;
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let palette = app.visual.palette();
    if cache.palette != Some(palette) || cache.colors.len() != inner.height as usize {
        cache.palette = Some(palette);
        cache.colors = (0..inner.height)
            .map(|y| {
                let c = crate::artwork::palette_at(
                    &palette,
                    y as f64 / inner.height.saturating_sub(1).max(1) as f64,
                );
                Color::Rgb(c[0], c[1], c[2])
            })
            .collect();
    }
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let bars = (inner.width as usize / 3).clamp(1, 32);
    let width = (inner.width as usize - (bars - 1)) / bars;
    let used = bars * width + bars - 1;
    let start = inner.x + (inner.width - used as u16) / 2;
    for bar in 0..bars {
        let from = bar * 32 / bars;
        let to = ((bar + 1) * 32 / bars).max(from + 1);
        let level = app.visual.levels[from..to]
            .iter()
            .copied()
            .fold(0f32, f32::max);
        let units = (level * inner.height as f32 * 8.).round() as usize;
        for y in 0..inner.height {
            let fill = units
                .saturating_sub((inner.height - y - 1) as usize * 8)
                .min(8);
            for x in 0..width {
                frame.buffer_mut()[(start + (bar * (width + 1) + x) as u16, inner.y + y)]
                    .set_char(BLOCKS[fill])
                    .set_fg(cache.colors[y as usize]);
            }
        }
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
    render_library_table(
        frame,
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
        app.follow_selection,
        app.entries.len(),
    );
}

fn render_library_table(
    frame: &mut Frame,
    table: Table<'_>,
    area: Rect,
    state: &mut TableState,
    follow_selection: bool,
    row_count: usize,
) {
    let selected = state.selected();
    if !follow_selection {
        // One header row and two borders; library rows are one terminal line each.
        let visible = area.height.saturating_sub(3) as usize;
        let offset = state.offset().min(row_count.saturating_sub(visible));
        *state.offset_mut() = offset;
        if selected.is_some_and(|index| index < offset || index >= offset + visible) {
            // Ratatui otherwise scrolls back to the selected row during rendering.
            *state.selected_mut() = None;
        }
    }
    frame.render_stateful_widget(
        table.highlight_spacing(HighlightSpacing::Always),
        area,
        state,
    );
    *state.selected_mut() = selected;
}
fn player(frame: &mut Frame, app: &mut App, area: Rect) {
    let rgb = crate::artwork::palette_at(&app.visual.palette(), 0.5);
    let accent = Color::Rgb(rgb[0], rgb[1], rgb[2]);
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
            .gauge_style(Style::new().fg(accent).bg(Color::DarkGray))
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
    frame.render_widget(Paragraph::new(volume).fg(accent), rows[3]);
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

    fn draw_scrolled_table(
        terminal: &mut Terminal<TestBackend>,
        state: &mut TableState,
        follow: bool,
    ) {
        terminal
            .draw(|frame| {
                let rows = (0..20).map(|i| Row::new([format!("Track {i:02}")]));
                let table = Table::new(rows, [Constraint::Min(1)])
                    .block(Block::bordered())
                    .header(Row::new(["Tracks"]))
                    .highlight_symbol("› ");
                render_library_table(frame, table, frame.area(), state, follow, 20);
            })
            .unwrap();
    }

    #[test]
    fn viewport_scroll_preserves_selection_across_redraws() {
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        let mut state = TableState::default().with_selected(Some(0)).with_offset(8);
        for _ in 0..3 {
            draw_scrolled_table(&mut terminal, &mut state, false);
            assert_eq!(state.selected(), Some(0));
            assert_eq!(state.offset(), 8);
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("Track 08"));
            assert!(!text.contains("Track 00"));
            assert!(!text.contains('›'));
        }
        // Keyboard navigation returns to the retained selection, not the viewport's top row.
        state.select(Some(1));
        draw_scrolled_table(&mut terminal, &mut state, true);
        assert_eq!(state.selected(), Some(1));
        assert!(state.offset() <= 1);
    }

    #[test]
    fn viewport_keeps_visible_highlight_and_clamps_after_resize() {
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        let mut state = TableState::default()
            .with_selected(Some(18))
            .with_offset(usize::MAX);
        draw_scrolled_table(&mut terminal, &mut state, false);
        assert_eq!(state.offset(), 15);
        assert_eq!(state.selected(), Some(18));
        assert_eq!(terminal.backend().buffer()[(1, 5)].symbol(), "›");
        terminal.backend_mut().resize(30, 12);
        draw_scrolled_table(&mut terminal, &mut state, false);
        assert_eq!(state.offset(), 11);
        assert_eq!(state.selected(), Some(18));
    }
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
