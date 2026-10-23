use crate::{
    core::{Album, Config, Song},
    library::Library,
    player::Player,
    state::PlaybackState,
    transfer::Manager,
};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{DefaultTerminal, layout::Rect, widgets::TableState};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const PAGES: [&str; 4] = ["Albums", "Downloaded", "Queue", "Transfers"];
#[derive(Clone, Debug)]
pub enum Entry {
    Album(Album),
    Song {
        song: Song,
        index: usize,
        status: String,
    },
}
impl Entry {
    pub fn song(&self) -> Option<&Song> {
        match self {
            Self::Song { song, .. } => Some(song),
            _ => None,
        }
    }
    pub fn title(&self) -> &str {
        match self {
            Self::Album(a) => &a.title,
            Self::Song { song, .. } => &song.title,
        }
    }
    pub fn artist(&self) -> &str {
        match self {
            Self::Album(a) => &a.artist,
            Self::Song { song, .. } => &song.artist,
        }
    }
    fn id(&self) -> String {
        match self {
            Self::Album(a) => format!("a:{}", a.id),
            Self::Song { song, index, .. } => format!("s:{}:{index}", song.id),
        }
    }
}
enum Message {
    Albums(Result<Vec<Album>>),
    Songs {
        id: String,
        generation: u64,
        download: bool,
        result: Result<Vec<Song>>,
    },
    Action(Result<()>),
}
#[derive(Default)]
pub struct Regions {
    pub tabs: Vec<(Rect, usize)>,
    pub table: Rect,
    pub seek: Rect,
    pub controls: Vec<(Rect, KeyCode)>,
    pub volume: Rect,
}
pub struct App {
    pub lib: Arc<Library>,
    pub transfers: Arc<Manager>,
    player: Player,
    pub state: PlaybackState,
    pub page: usize,
    pub album: Option<Album>,
    albums: Vec<Album>,
    songs: Vec<Song>,
    pub entries: Vec<Entry>,
    pub table: TableState,
    pub query: String,
    pub filtering: bool,
    pub help: bool,
    pub help_scroll: u16,
    pub loading: bool,
    pub notice: String,
    confirm: String,
    pub paused: bool,
    pub loaded: bool,
    pub buffering: bool,
    pub spatial: bool,
    pub duration: f64,
    pub regions: Regions,
    history: Vec<i32>,
    active: String,
    ended: bool,
    failed: bool,
    request_generation: u64,
    tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    last_save: Instant,
    last_click: Option<(usize, Instant)>,
    pub focused: bool,
}
impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let spatial = cfg.player.spatial_audio;
        let lib = Arc::new(Library::new(cfg)?);
        let state = PlaybackState::read(lib.cache_dir());
        let duration = state.current().map_or(0., |s| s.duration);
        let mut player = Player::new(spatial)?;
        player.volume(state.volume)?;
        let spatial = player.spatial();
        let transfers = Arc::new(Manager::new(lib.clone())?);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            lib,
            transfers,
            player,
            state,
            page: 0,
            album: None,
            albums: vec![],
            songs: vec![],
            entries: vec![],
            table: TableState::default(),
            query: String::new(),
            filtering: false,
            help: false,
            help_scroll: 0,
            loading: true,
            notice: String::new(),
            confirm: String::new(),
            paused: true,
            loaded: false,
            buffering: false,
            spatial,
            duration,
            regions: Regions::default(),
            history: vec![],
            active: String::new(),
            ended: false,
            failed: false,
            request_generation: 0,
            tx,
            rx,
            last_save: Instant::now(),
            last_click: None,
            focused: true,
        };
        app.fetch_albums(false);
        app.sync_entries();
        Ok(app)
    }
    pub fn run(&mut self, terminal: &mut DefaultTerminal, quit: Arc<AtomicBool>) -> Result<()> {
        struct MouseGuard;
        impl Drop for MouseGuard {
            fn drop(&mut self) {
                let _ = execute!(
                    std::io::stdout(),
                    DisableMouseCapture,
                    event::DisableFocusChange
                );
            }
        }
        let _guard = MouseGuard;
        execute!(
            std::io::stdout(),
            EnableMouseCapture,
            event::EnableFocusChange
        )?;
        let mut last_tick = Instant::now();
        loop {
            if quit.load(Ordering::Relaxed) {
                break;
            }
            self.messages();
            if last_tick.elapsed() >= Duration::from_millis(200) {
                self.tick();
                last_tick = Instant::now();
            }
            terminal.draw(|f| crate::ui::draw(f, self))?;
            if event::poll(Duration::from_millis(if self.focused { 50 } else { 200 }))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if self.key(k) {
                            break;
                        }
                    }
                    Event::Mouse(m) => self.mouse(m),
                    Event::FocusGained => self.focused = true,
                    Event::FocusLost => self.focused = false,
                    _ => {}
                }
            }
        }
        Ok(())
    }
    pub fn shutdown(&mut self) {
        let _ = self.player.stop();
        self.transfers.release(&self.active);
        self.active.clear();
        self.transfers.close();
    }
    fn fetch_albums(&mut self, refresh: bool) {
        self.loading = true;
        let lib = self.lib.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Message::Albums(lib.albums(refresh)));
        });
    }
    fn fetch_songs(&mut self, album: &Album, download: bool) {
        let lib = self.lib.clone();
        let tx = self.tx.clone();
        let id = album.id.clone();
        let generation = self.request_generation;
        thread::spawn(move || {
            let result = lib.songs(&id);
            let _ = tx.send(Message::Songs {
                id,
                generation,
                download,
                result,
            });
        });
    }
    fn messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Message::Albums(result) => {
                    self.loading = false;
                    match result {
                        Ok(a) => self.albums = a,
                        Err(e) => self.error(e),
                    }
                }
                Message::Songs {
                    id,
                    generation,
                    download,
                    result,
                } => {
                    if download {
                        match result {
                            Ok(songs) => {
                                for song in songs {
                                    let r = self.transfers.download(&song);
                                    self.check(r);
                                }
                                self.notice = "Download queued".into();
                            }
                            Err(e) => self.error(e),
                        }
                    } else if generation == self.request_generation
                        && self.album.as_ref().is_some_and(|a| a.id == id)
                    {
                        self.loading = false;
                        match result {
                            Ok(s) => self.songs = s,
                            Err(e) => self.error(e),
                        }
                    }
                }
                Message::Action(result) => self.check(result),
            }
            self.sync_entries();
        }
    }
    fn tick(&mut self) {
        let snap = self.player.snapshot();
        if snap.generation == self.player.generation() && !self.active.is_empty() {
            if snap.status == 3 && !self.failed {
                self.failed = true;
                self.buffering = false;
                self.paused = true;
                self.loaded = false;
                self.notice = "Playback failed · space retry".into();
            } else if snap.status == 2 {
                if !self.failed {
                    self.loaded = true;
                    self.paused = snap.paused;
                    self.buffering = snap.buffering;
                }
                if snap.duration.is_finite() && snap.duration > 0. {
                    self.duration = snap.duration;
                }
                if snap.position.is_finite() {
                    self.state.position = snap.position.max(0.);
                }
                if snap.ended && !self.ended && self.loaded {
                    self.ended = true;
                    self.next(false);
                } else if !snap.ended {
                    self.ended = false;
                }
            } else if snap.status == 1 {
                self.buffering = snap.buffering;
            }
        }
        if self.page == 1 || self.page == 3 {
            self.sync_entries();
        }
        if self.last_save.elapsed() >= Duration::from_secs(5) {
            let r = self.save();
            self.check(r);
        }
    }
    pub fn save(&mut self) -> Result<()> {
        self.last_save = Instant::now();
        self.state.write(self.lib.cache_dir())
    }
    fn error(&mut self, error: anyhow::Error) {
        self.notice = clean(&error.to_string());
    }
    fn check(&mut self, result: Result<()>) {
        if let Err(e) = result {
            self.error(e);
        }
    }
    pub fn sync_entries(&mut self) {
        let selected = self.selected().map(Entry::id);
        let mut entries = vec![];
        if self.page == 2 {
            entries = self
                .state
                .queue
                .iter()
                .enumerate()
                .map(|(i, s)| Entry::Song {
                    song: s.clone(),
                    index: i,
                    status: if i as i32 == self.state.index {
                        "Playing".into()
                    } else {
                        String::new()
                    },
                })
                .collect();
        } else if self.page == 3 {
            entries = self
                .transfers
                .snapshot()
                .into_iter()
                .map(|s| Entry::Song {
                    song: s.song,
                    index: 0,
                    status: format!(
                        "{}{}{}",
                        s.state,
                        s.bytes
                            .saturating_mul(100)
                            .checked_div(s.total)
                            .map(|percent| format!(" {percent}%"))
                            .unwrap_or_default(),
                        if s.error.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", s.error)
                        }
                    ),
                })
                .collect();
        } else if let Some(album) = &self.album {
            let songs = if self.page == 1 {
                self.lib.downloaded_songs(&album.id)
            } else {
                self.songs.clone()
            };
            entries = songs
                .into_iter()
                .map(|s| {
                    let status = if self.lib.local_path(&s.id).is_some() {
                        "Local".into()
                    } else {
                        String::new()
                    };
                    Entry::Song {
                        song: s,
                        index: 0,
                        status,
                    }
                })
                .collect();
        } else {
            let mut albums = if self.page == 1 {
                self.lib.downloaded_albums()
            } else {
                self.albums.clone()
            };
            albums.sort_by(|a, b| {
                let recent = |a: &Album| {
                    a.last_played
                        .max(*self.state.recent.get(&a.id).unwrap_or(&0.))
                };
                recent(b)
                    .total_cmp(&recent(a))
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            });
            entries.extend(albums.into_iter().map(Entry::Album));
        }
        let query = self.query.trim().to_lowercase();
        self.entries = entries
            .into_iter()
            .filter(|e| {
                query.is_empty()
                    || format!(
                        "{} {} {}",
                        e.title(),
                        e.artist(),
                        e.song().map_or("", |s| s.album_title.as_str())
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        let idx = selected
            .and_then(|id| self.entries.iter().position(|e| e.id() == id))
            .unwrap_or(self.table.selected().unwrap_or(0));
        self.table.select(if self.entries.is_empty() {
            None
        } else {
            Some(idx.min(self.entries.len() - 1))
        });
    }
    pub fn selected(&self) -> Option<&Entry> {
        self.table.selected().and_then(|i| self.entries.get(i))
    }
    fn set_page(&mut self, page: usize) {
        self.page = page;
        self.album = None;
        self.songs.clear();
        self.query.clear();
        self.filtering = false;
        self.confirm.clear();
        self.last_click = None;
        self.loading = false;
        self.request_generation += 1;
        self.table = TableState::default();
        self.sync_entries();
    }
    fn activate(&mut self) {
        self.last_click = None;
        match self.selected().cloned() {
            Some(Entry::Album(a)) => {
                self.album = Some(a.clone());
                self.songs.clear();
                self.query.clear();
                self.table = TableState::default();
                self.request_generation += 1;
                self.loading = self.page == 0;
                if self.page == 0 {
                    self.fetch_songs(&a, false);
                }
                self.sync_entries();
            }
            Some(Entry::Song { index, .. }) if self.page == 2 => self.play(index as i32, false, 0.),
            Some(Entry::Song { song, .. }) if self.page != 3 => {
                self.state.queue = self
                    .entries
                    .iter()
                    .filter_map(|e| e.song().cloned())
                    .collect();
                self.history.clear();
                let i = self
                    .state
                    .queue
                    .iter()
                    .position(|s| s.id == song.id)
                    .unwrap_or(0);
                self.play(i as i32, false, 0.);
            }
            _ => {}
        }
    }
    fn play(&mut self, index: i32, paused: bool, position: f64) {
        if index < 0 || index as usize >= self.state.queue.len() {
            return;
        }
        if !self.active.is_empty() {
            self.transfers.release(&self.active);
        }
        self.active.clear();
        self.state.index = index;
        self.state.position = position;
        self.paused = paused;
        self.loaded = false;
        self.buffering = true;
        self.ended = false;
        self.failed = false;
        let song = self.state.queue[index as usize].clone();
        self.duration = song.duration;
        if !paused {
            self.state.recent.insert(
                song.album_id.clone(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
            );
        }
        let result = self.transfers.source(&song).and_then(|source| {
            self.active = song.id;
            self.player.load(&source, paused, position)
        });
        if let Err(e) = result {
            let _ = self.player.stop();
            self.buffering = false;
            self.paused = true;
            self.error(e);
        }
        self.sync_entries();
    }
    fn next(&mut self, manual: bool) {
        if self.state.queue.is_empty() {
            return;
        }
        if !manual && self.state.repeat == 2 {
            self.play(self.state.index, false, 0.);
            return;
        }
        if self.state.shuffle && self.state.queue.len() > 1 {
            self.history.push(self.state.index);
            let mut n = rand::random_range(0..self.state.queue.len() - 1) as i32;
            if n >= self.state.index {
                n += 1;
            }
            self.play(n, false, 0.);
            return;
        }
        let mut n = self.state.index + 1;
        if n as usize >= self.state.queue.len() {
            if self.state.repeat == 1 {
                n = 0;
            } else {
                if !manual {
                    self.state.position = 0.;
                    self.paused = true;
                    self.loaded = false;
                    self.buffering = false;
                    self.transfers.release(&self.active);
                    self.active.clear();
                }
                return;
            }
        }
        self.play(n, false, 0.);
    }
    fn previous(&mut self) {
        if self.loaded && self.state.position > 3. {
            self.seek(0., true);
            return;
        }
        let index = self.history.pop().unwrap_or((self.state.index - 1).max(0));
        self.play(index, false, 0.);
    }
    fn toggle(&mut self) {
        if self.state.current().is_none() {
            self.activate();
        } else if !self.loaded {
            self.play(self.state.index, false, self.state.position);
        } else {
            self.paused = !self.paused;
            let r = self.player.pause(self.paused);
            self.check(r);
        }
    }
    fn seek(&mut self, seconds: f64, absolute: bool) {
        self.state.position = if absolute {
            seconds
        } else {
            self.state.position + seconds
        }
        .clamp(0., self.duration.max(0.));
        if self.loaded {
            let r = self.player.seek(seconds, absolute);
            self.check(r);
        }
        self.ended = false;
    }
    fn volume(&mut self, delta: f64) {
        self.state.volume = (self.state.volume + delta).clamp(0., 100.);
        let r = self.player.volume(self.state.volume);
        self.check(r);
    }
    fn download(&mut self) {
        match self.selected().cloned() {
            Some(Entry::Song { song, .. }) => {
                if self.lib.local_path(&song.id).is_some() {
                    self.notice = "Already local".into();
                } else {
                    match self.transfers.download(&song) {
                        Ok(()) => self.notice = "Download queued".into(),
                        Err(e) => self.error(e),
                    }
                }
            }
            Some(Entry::Album(a)) => self.fetch_songs(&a, true),
            _ => {}
        }
    }
    fn clear(&mut self) {
        self.transfers.release(&self.active);
        self.active.clear();
        let r = self.player.stop();
        self.check(r);
        self.loaded = false;
        self.paused = true;
        self.buffering = false;
        self.state.queue.clear();
        self.state.index = -1;
        self.state.position = 0.;
        self.duration = 0.;
        self.history.clear();
        self.sync_entries();
    }
    fn cancel(&mut self) {
        if self.page == 2 {
            if self.state.index >= 0 {
                self.state.queue.truncate(self.state.index as usize + 1);
            }
        } else {
            match self.selected().cloned() {
                Some(Entry::Song { song, .. }) => self.transfers.cancel(&song.id),
                Some(Entry::Album(a)) => {
                    for s in self.transfers.snapshot() {
                        if s.song.album_id == a.id {
                            self.transfers.cancel(&s.song.id);
                        }
                    }
                }
                _ => {}
            }
        }
        self.sync_entries();
    }
    fn remove(&mut self) {
        let Some(entry) = self.selected().cloned() else {
            return;
        };
        if self.page == 2 {
            if let Entry::Song { index, .. } = entry {
                let current = index as i32 == self.state.index;
                self.state.queue.remove(index);
                self.history.clear();
                if (index as i32) < self.state.index {
                    self.state.index -= 1;
                }
                if current {
                    if index < self.state.queue.len() {
                        self.play(index as i32, false, 0.);
                    } else {
                        self.transfers.release(&self.active);
                        self.active.clear();
                        let r = self.player.stop();
                        self.check(r);
                        self.loaded = false;
                        self.paused = true;
                        self.buffering = false;
                        self.state.index = -1;
                        self.state.position = 0.;
                        self.duration = 0.;
                    }
                }
            }
            self.sync_entries();
            return;
        }
        if self.page == 3 {
            self.cancel();
            return;
        }
        let id = entry.id();
        if self.confirm != id {
            self.confirm = id;
            self.notice = "Press x again to remove local download".into();
            return;
        }
        self.confirm.clear();
        self.notice.clear();
        let songs = match entry {
            Entry::Song { song, .. } => vec![song],
            Entry::Album(a) => self.lib.downloaded_songs(&a.id),
        };
        let mut ids = vec![];
        for song in songs {
            if !self.lib.is_owned(&song.id) {
                self.notice = "External files are read-only".into();
                continue;
            }
            if self.active == song.id {
                self.notice = "Stop playback before removing this track".into();
                continue;
            }
            self.transfers.cancel(&song.id);
            ids.push(song.id);
        }
        if !ids.is_empty() {
            let lib = self.lib.clone();
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = ids.iter().try_for_each(|id| lib.remove(id));
                let _ = tx.send(Message::Action(result));
            });
        }
    }
    fn move_queue(&mut self, down: bool) {
        if self.page != 2 {
            return;
        }
        let Some(Entry::Song { index: i, .. }) = self.selected().cloned() else {
            return;
        };
        let j = if down { i + 1 } else { i.saturating_sub(1) };
        if j >= self.state.queue.len() || i == j {
            return;
        }
        self.state.queue.swap(i, j);
        if self.state.index == i as i32 {
            self.state.index = j as i32;
        } else if self.state.index == j as i32 {
            self.state.index = i as i32;
        }
        self.history.clear();
        self.sync_entries();
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| matches!(e,Entry::Song {index,..} if *index==j))
        {
            self.table.select(Some(idx));
        }
    }
    fn navigate(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let i = self
            .table
            .selected()
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.entries.len() - 1);
        self.table.select(Some(i));
        self.confirm.clear();
    }
    pub fn key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return true;
        }
        if self.filtering {
            match k.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.query.clear();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c)
                    if !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(c)
                }
                _ => {}
            }
            self.table = TableState::default();
            self.sync_entries();
            return false;
        }
        if self.help {
            match k.code {
                KeyCode::Char('q') => return true,
                KeyCode::Char('?') | KeyCode::Esc => self.help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(30)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(5).min(30),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(5),
                _ => {}
            }
            return false;
        }
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => {
                self.help = true;
                self.help_scroll = 0;
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.confirm.clear();
            }
            KeyCode::Esc => {
                self.confirm.clear();
                if !self.query.is_empty() {
                    self.query.clear();
                    self.sync_entries();
                } else if let Some(a) = self.album.take() {
                    self.request_generation += 1;
                    self.loading = false;
                    self.sync_entries();
                    if let Some(i) = self
                        .entries
                        .iter()
                        .position(|e| matches!(e,Entry::Album(b) if a.id==b.id))
                    {
                        self.table.select(Some(i));
                    }
                } else {
                    self.notice.clear();
                }
            }
            KeyCode::Char(c @ '1'..='4') => self.set_page(c as usize - '1' as usize),
            KeyCode::Tab => self.set_page((self.page + 1) % 4),
            KeyCode::BackTab => self.set_page((self.page + 3) % 4),
            KeyCode::Enter => self.activate(),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('n') => self.next(true),
            KeyCode::Char('p') => self.previous(),
            KeyCode::Left | KeyCode::Char('h') => self.seek(-5., false),
            KeyCode::Right | KeyCode::Char('l') => self.seek(5., false),
            KeyCode::Char('+' | '=') => self.volume(5.),
            KeyCode::Char('-') => self.volume(-5.),
            KeyCode::Char('o') => {
                let value = !self.spatial;
                let r = self.player.set_spatial(value);
                if r.is_ok() {
                    self.spatial = value;
                }
                self.check(r);
            }
            KeyCode::Char('s') => self.state.shuffle = !self.state.shuffle,
            KeyCode::Char('r') => self.state.repeat = (self.state.repeat + 1) % 3,
            KeyCode::Char('R') => self.fetch_albums(true),
            KeyCode::Char('d') => self.download(),
            KeyCode::Char('C') => self.clear(),
            KeyCode::Char('c') => self.cancel(),
            KeyCode::Char(c @ ('a' | 'N')) => {
                if self.page != 2
                    && let Some(song) = self.selected().and_then(Entry::song).cloned()
                {
                    let i = if c == 'N' && self.state.index >= 0 {
                        self.state.index as usize + 1
                    } else {
                        self.state.queue.len()
                    };
                    self.state.queue.insert(i, song);
                    self.history.clear();
                    self.notice = "Added to queue".into();
                }
            }
            KeyCode::Char('x') => self.remove(),
            KeyCode::Char('J') => self.move_queue(true),
            KeyCode::Char('K') => self.move_queue(false),
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::PageDown => {
                self.navigate(self.regions.table.height.saturating_sub(3) as isize)
            }
            KeyCode::PageUp => {
                self.navigate(-(self.regions.table.height.saturating_sub(3) as isize))
            }
            KeyCode::Home | KeyCode::Char('g') => self.navigate(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.navigate(isize::MAX),
            _ => {}
        }
        false
    }
    fn mouse(&mut self, m: MouseEvent) {
        if self.help {
            match m.kind {
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(2).min(30)
                }
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(2),
                _ => {}
            }
            return;
        }
        let pos = (m.column, m.row).into();
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if self.regions.volume.contains(pos) {
                    self.volume(if up { 5. } else { -5. });
                } else if self.regions.table.contains(pos) {
                    self.navigate(if up { -2 } else { 2 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, page)) = self
                    .regions
                    .tabs
                    .iter()
                    .find(|(r, _)| r.contains(pos))
                    .copied()
                {
                    self.set_page(page);
                    return;
                }
                if let Some((_, code)) = self
                    .regions
                    .controls
                    .iter()
                    .find(|(r, _)| r.contains(pos))
                    .copied()
                {
                    self.key(KeyEvent::new(code, KeyModifiers::NONE));
                    return;
                }
                if self.regions.seek.contains(pos) && self.duration > 0. {
                    let r = self.regions.seek;
                    self.seek(
                        (m.column - r.x) as f64 / r.width.saturating_sub(1).max(1) as f64
                            * self.duration,
                        true,
                    );
                    return;
                }
                let r = self.regions.table;
                if r.contains(pos) && m.row >= r.y + 2 && m.row < r.bottom().saturating_sub(1) {
                    let i = self.table.offset() + (m.row - r.y - 2) as usize;
                    if i < self.entries.len() {
                        self.confirm.clear();
                        self.table.select(Some(i));
                        let double = self.last_click.is_some_and(|(last, t)| {
                            last == i && t.elapsed() < Duration::from_millis(400)
                        });
                        self.last_click = Some((i, Instant::now()));
                        if double {
                            self.activate();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
pub fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}
pub fn duration(seconds: f64) -> String {
    let n = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    format!("{}:{:02}", n / 60, n % 60)
}
