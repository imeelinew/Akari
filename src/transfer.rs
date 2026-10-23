use crate::{core::Song, library::Library};
use anyhow::{Context, Result, anyhow, bail};
use rand::random;
use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tiny_http::{Header, Method, Request, Response as HttpResponse, Server, StatusCode};
use url::Url;

const IDLE: &str = "idle";
const QUEUED: &str = "queued";
const DOWNLOADING: &str = "downloading";
const STREAMING: &str = "streaming";
const SAVING: &str = "saving";
const COMPLETE: &str = "complete";
const ERROR: &str = "error";
const CANCELED: &str = "canceled";

const MANUAL_CONCURRENCY: usize = 3;
const PLAYBACK_CHUNK_SIZE: u64 = 1 << 20;
const MANUAL_CHUNK_SIZE: u64 = 4 << 20;
// Chunk deadlines must allow slow lossless downloads; shutdown has its own bound.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);
const COPY_BUFFER_SIZE: usize = 32 << 10;
const NETWORK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// A stable transfer row for the UI.
#[derive(Clone, Debug)]
pub struct Status {
    pub song: Song,
    pub state: String,
    pub bytes: u64,
    pub total: u64,
    pub error: String,
}

trait LibraryBackend: Send + Sync {
    fn local_path(&self, song_id: &str) -> Option<PathBuf>;
    fn stream_url(&self, song_id: &str) -> Result<String>;
    fn download_url(&self, song_id: &str) -> Result<String>;
    fn http_client(&self) -> Client;
    fn save_download(&self, song: &Song, temp: &Path, extension: &str) -> Result<()>;
}

impl LibraryBackend for Library {
    fn local_path(&self, song_id: &str) -> Option<PathBuf> {
        Library::local_path(self, song_id)
    }

    fn stream_url(&self, song_id: &str) -> Result<String> {
        Library::stream_url(self, song_id)
    }

    fn download_url(&self, song_id: &str) -> Result<String> {
        Library::download_url(self, song_id)
    }

    fn http_client(&self) -> Client {
        Library::http_client(self)
    }

    fn save_download(&self, song: &Song, temp: &Path, extension: &str) -> Result<()> {
        Library::save_download(self, song, temp, extension)
    }
}

/// Serves lazy playback through a loopback proxy and owns background downloads.
pub struct Manager {
    inner: Arc<Inner>,
    close_lock: Mutex<()>,
    server_thread: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    lib: Arc<dyn LibraryBackend>,
    client: Client,
    temp_dir: Mutex<Option<TempDir>>,
    temp_path: PathBuf,
    base_url: String,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    workers: Mutex<Vec<WorkerHandle>>,
    requests: Mutex<Vec<JoinHandle<()>>>,
    manual_gate: ManualGate,
    closed: AtomicBool,
}

struct Entry {
    id: String,
    cache_path: PathBuf,
    fetch_lock: Mutex<()>,
    state: Mutex<EntryState>,
}

struct EntryState {
    song: Song,
    total: Option<u64>,
    ranges: Vec<ByteRange>,
    content_type: String,
    extension: String,
    disposition: String,
    playback: Option<Arc<PlaybackRun>>,
    manual_retained: bool,
    manual_job: Option<Arc<ManualJob>>,
    manual_workers: usize,
    active_handlers: usize,
    discard_requested: bool,
    state: &'static str,
    last_error: String,
    saving: bool,
    saved: bool,
    save_attempted: bool,
}

struct PlaybackRun {
    token: String,
    source_url: String,
    cancel: CancelToken,
    leases: AtomicUsize,
}

struct ManualJob {
    source_url: String,
    cancel: CancelToken,
}

#[derive(Clone)]
struct CancelToken {
    canceled: Arc<AtomicBool>,
}

impl CancelToken {
    fn new() -> Self {
        Self {
            canceled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
    }

    fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Debug)]
struct RangeNotSatisfiable {
    total: Option<u64>,
}

impl std::fmt::Display for RangeNotSatisfiable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source range not satisfiable")
    }
}

impl std::error::Error for RangeNotSatisfiable {}

#[derive(Debug)]
enum ClientRange {
    Full,
    Bounded { start: u64, end: Option<u64> },
    Suffix(u64),
}

#[derive(Debug)]
struct ContentRange {
    start: Option<u64>,
    end: Option<u64>,
    total: Option<u64>,
}

#[derive(Debug)]
struct FetchResult {
    end: u64,
    total: Option<u64>,
    content_type: String,
    extension: String,
    disposition: String,
}

struct ManualGate {
    active: Mutex<usize>,
    changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerKind {
    Network,
    Save,
}

struct WorkerHandle {
    kind: WorkerKind,
    handle: JoinHandle<()>,
}

impl ManualGate {
    fn new() -> Self {
        Self {
            active: Mutex::new(0),
            changed: std::sync::Condvar::new(),
        }
    }

    fn acquire(&self, cancel: &CancelToken) -> bool {
        let mut active = lock(&self.active);
        while *active >= MANUAL_CONCURRENCY {
            if cancel.is_canceled() {
                return false;
            }
            let (next, _) = self
                .changed
                .wait_timeout(active, Duration::from_millis(25))
                .unwrap_or_else(|error| error.into_inner());
            active = next;
        }
        if cancel.is_canceled() {
            return false;
        }
        *active += 1;
        true
    }

    fn release(&self) {
        let mut active = lock(&self.active);
        *active = active.saturating_sub(1);
        self.changed.notify_one();
    }
}

impl Manager {
    /// Starts a loopback proxy on an ephemeral IPv4 port.
    pub fn new(lib: Arc<Library>) -> Result<Self> {
        Self::with_backend(lib)
    }

    fn with_backend(lib: Arc<dyn LibraryBackend>) -> Result<Self> {
        let temp_dir = tempfile::tempdir().context("create playback cache directory")?;
        let temp_path = temp_dir.path().to_path_buf();
        let server = Server::http("127.0.0.1:0").map_err(|error| {
            anyhow!("listen on loopback: {}", sanitize_error(&error.to_string()))
        })?;
        let address = server
            .server_addr()
            .to_ip()
            .context("invalid loopback address")?;
        let inner = Arc::new(Inner {
            client: lib.http_client(),
            lib,
            temp_dir: Mutex::new(Some(temp_dir)),
            temp_path,
            base_url: format!("http://127.0.0.1:{}", address.port()),
            entries: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            manual_gate: ManualGate::new(),
            closed: AtomicBool::new(false),
        });

        let server = Arc::new(server);
        let server_inner = inner.clone();
        let server_thread = thread::Builder::new()
            .name("akari-transfer-proxy".into())
            .spawn(move || run_server(server, server_inner))
            .context("start loopback proxy")?;

        Ok(Self {
            inner,
            close_lock: Mutex::new(()),
            server_thread: Mutex::new(Some(server_thread)),
        })
    }

    /// Returns a local file when available, otherwise a tokenized loopback URL.
    ///
    /// This method performs no network I/O.
    pub fn source(&self, song: &Song) -> Result<String> {
        if song.id.trim().is_empty() {
            bail!("song ID is empty");
        }
        self.ensure_open()?;
        if let Some(path) = self.local_path(&song.id) {
            return Ok(path.to_string_lossy().into_owned());
        }

        let source_url = raw_source_url(&self.inner.lib.stream_url(&song.id)?)?;
        let fresh_token = random_token();
        let entry = {
            let mut entries = lock(&self.inner.entries);
            if self.inner.closed.load(Ordering::Acquire) {
                bail!("transfer manager is closed");
            }
            ensure_entry(&self.inner, &mut entries, song)
        };

        let (token, local) = {
            let mut state = lock(&entry.state);
            if self.inner.closed.load(Ordering::Acquire) {
                bail!("transfer manager is closed");
            }
            state.song = song.clone();
            state.discard_requested = false;
            let run = if let Some(run) = &state.playback {
                run.clone()
            } else {
                let run = Arc::new(PlaybackRun {
                    token: fresh_token,
                    source_url,
                    cancel: CancelToken::new(),
                    leases: AtomicUsize::new(0),
                });
                state.playback = Some(run.clone());
                run
            };
            let token = run.token.clone();
            run.leases.fetch_add(1, Ordering::AcqRel);
            let local = self.local_path(&song.id);
            if local.is_some() {
                run.leases.fetch_sub(1, Ordering::AcqRel);
                if state.playback.as_ref().is_some_and(|current| {
                    Arc::ptr_eq(current, &run) && current.leases.load(Ordering::Acquire) == 0
                }) {
                    run.cancel.cancel();
                    state.playback = None;
                }
            }
            (token, local)
        };

        if let Some(path) = local {
            reconcile(&self.inner, &entry);
            return Ok(path.to_string_lossy().into_owned());
        }

        Ok(format!(
            "{}/stream?id={}&token={}",
            self.inner.base_url,
            encode_query(&song.id),
            encode_query(&token)
        ))
    }

    /// Retains a song and starts a background raw download.
    pub fn download(&self, song: &Song) -> Result<()> {
        if song.id.trim().is_empty() {
            bail!("song ID is empty");
        }
        self.ensure_open()?;
        if self.local_path(&song.id).is_some() {
            return Ok(());
        }

        let source_url = raw_source_url(&self.inner.lib.download_url(&song.id)?)?;
        let entry = {
            let mut entries = lock(&self.inner.entries);
            if self.inner.closed.load(Ordering::Acquire) {
                bail!("transfer manager is closed");
            }
            ensure_entry(&self.inner, &mut entries, song)
        };

        let job = {
            let mut state = lock(&entry.state);
            state.song = song.clone();
            state.manual_retained = true;
            state.discard_requested = false;
            state.last_error.clear();
            state.save_attempted = false;
            if state
                .manual_job
                .as_ref()
                .is_some_and(|job| !job.cancel.is_canceled())
            {
                return Ok(());
            }
            if state.is_full() {
                state.state = SAVING;
                None
            } else {
                let job = Arc::new(ManualJob {
                    source_url,
                    cancel: CancelToken::new(),
                });
                state.manual_job = Some(job.clone());
                state.manual_workers += 1;
                state.state = QUEUED;
                Some(job)
            }
        };

        let Some(job) = job else {
            maybe_save(&self.inner, &entry);
            return Ok(());
        };

        let inner = self.inner.clone();
        let entry_for_worker = entry.clone();
        let job_for_worker = job.clone();
        if let Err(error) = spawn_worker(
            &self.inner,
            "akari-transfer-download",
            WorkerKind::Network,
            move || {
                run_manual(inner, entry_for_worker, job_for_worker);
            },
        ) {
            let mut state = lock(&entry.state);
            if state
                .manual_job
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &job))
            {
                state.manual_job = None;
                state.manual_workers = 0;
                state.last_error = sanitize_error(&error.to_string());
                state.state = ERROR;
            }
            return Err(error).context("start download worker");
        }
        Ok(())
    }

    /// Cancels a retained manual download while leaving active playback intact.
    pub fn cancel(&self, id: &str) {
        let entry = lock(&self.inner.entries).get(id).cloned();
        let Some(entry) = entry else {
            return;
        };
        {
            let mut state = lock(&entry.state);
            state.manual_retained = false;
            state.discard_requested = true;
            state.last_error.clear();
            if let Some(job) = &state.manual_job {
                job.cancel.cancel();
            }
            if state.state == QUEUED || state.state == DOWNLOADING {
                state.state = CANCELED;
            }
        }
        reconcile(&self.inner, &entry);
    }

    /// Drops playback leases. A retained manual download continues.
    pub fn release(&self, id: &str) {
        let entry = lock(&self.inner.entries).get(id).cloned();
        let Some(entry) = entry else {
            return;
        };
        let retained = {
            let mut state = lock(&entry.state);
            if let Some(playback) = state.playback.take() {
                playback.cancel.cancel();
            }
            if state.manual_retained {
                true
            } else {
                state.discard_requested = true;
                if let Some(job) = &state.manual_job {
                    job.cancel.cancel();
                }
                false
            }
        };
        if retained {
            maybe_save(&self.inner, &entry);
        } else {
            reconcile(&self.inner, &entry);
        }
    }

    /// Returns a stable snapshot sorted like the Go manager.
    pub fn snapshot(&self) -> Vec<Status> {
        let entries: Vec<_> = lock(&self.inner.entries).values().cloned().collect();
        let mut statuses: Vec<_> = entries
            .iter()
            .map(|entry| lock(&entry.state).status())
            .collect();
        statuses.sort_by(|left, right| {
            (
                &left.song.artist,
                &left.song.album_title,
                left.song.track_number,
                &left.song.id,
            )
                .cmp(&(
                    &right.song.artist,
                    &right.song.album_title,
                    right.song.track_number,
                    &right.song.id,
                ))
        });
        statuses
    }

    /// Cancels all work, joins background threads, and removes partial files.
    pub fn close(&self) {
        let _close_guard = lock(&self.close_lock);
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        let entries: Vec<_> = lock(&self.inner.entries).values().cloned().collect();
        for entry in &entries {
            let state = lock(&entry.state);
            if let Some(playback) = &state.playback {
                playback.cancel.cancel();
            }
            if let Some(job) = &state.manual_job {
                job.cancel.cancel();
            }
        }

        if let Some(handle) = lock(&self.server_thread).take() {
            let _ = handle.join();
        }

        join_workers(
            &self.inner,
            WorkerKind::Save,
            Instant::now() + Duration::from_secs(60),
        );
        join_workers(
            &self.inner,
            WorkerKind::Network,
            Instant::now() + NETWORK_SHUTDOWN_TIMEOUT,
        );
        join_requests(&self.inner, Instant::now() + NETWORK_SHUTDOWN_TIMEOUT);

        if let Some(temp_dir) = lock(&self.inner.temp_dir).take() {
            let _ = temp_dir.close();
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            bail!("transfer manager is closed");
        }
        Ok(())
    }

    fn local_path(&self, song_id: &str) -> Option<PathBuf> {
        self.inner
            .lib
            .local_path(song_id)
            .filter(|path| path.is_file())
    }

    #[cfg(test)]
    fn cache_path(&self, song_id: &str) -> Option<PathBuf> {
        lock(&self.inner.entries)
            .get(song_id)
            .map(|entry| entry.cache_path.clone())
    }

    #[cfg(test)]
    fn temp_path(&self) -> PathBuf {
        self.inner.temp_path.clone()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_server(server: Arc<Server>, inner: Arc<Inner>) {
    while !inner.closed.load(Ordering::Acquire) {
        reap_finished_workers(&inner);
        match server.recv_timeout(Duration::from_millis(50)) {
            Ok(Some(request)) => {
                let request_inner = inner.clone();
                let result = spawn_request(&inner, move || {
                    handle_request(request_inner, request);
                });
                if result.is_err() {
                    // The request is dropped and the client gets a closed connection.
                }
            }
            Ok(None) => {}
            Err(_) if inner.closed.load(Ordering::Acquire) => break,
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn reap_finished_workers(inner: &Arc<Inner>) {
    let (finished, requests) = {
        let mut workers = lock(&inner.workers);
        let mut kept = Vec::with_capacity(workers.len());
        let mut ready = Vec::new();
        for worker in workers.drain(..) {
            if worker.handle.is_finished() {
                ready.push(worker.handle);
            } else {
                kept.push(worker);
            }
        }
        *workers = kept;
        let mut active_requests = lock(&inner.requests);
        let mut finished_requests = Vec::new();
        let mut kept_requests = Vec::with_capacity(active_requests.len());
        for handle in active_requests.drain(..) {
            if handle.is_finished() {
                finished_requests.push(handle);
            } else {
                kept_requests.push(handle);
            }
        }
        *active_requests = kept_requests;
        (ready, finished_requests)
    };
    for handle in finished {
        let _ = handle.join();
    }
    for handle in requests {
        let _ = handle.join();
    }
}

fn spawn_worker<F>(inner: &Arc<Inner>, name: &str, kind: WorkerKind, job: F) -> io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    if inner.closed.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "manager is closed",
        ));
    }
    let handle = thread::Builder::new().name(name.into()).spawn(job)?;
    lock(&inner.workers).push(WorkerHandle { kind, handle });
    Ok(())
}

fn spawn_request<F>(inner: &Arc<Inner>, job: F) -> io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    if inner.closed.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "manager is closed",
        ));
    }
    let handle = thread::Builder::new()
        .name("akari-transfer-request".into())
        .spawn(job)?;
    lock(&inner.requests).push(handle);
    Ok(())
}

fn join_workers(inner: &Arc<Inner>, kind: WorkerKind, deadline: Instant) {
    loop {
        let finished = {
            let mut workers = lock(&inner.workers);
            let mut ready = Vec::new();
            let mut kept = Vec::with_capacity(workers.len());
            for worker in workers.drain(..) {
                if worker.handle.is_finished() {
                    ready.push(worker.handle);
                } else {
                    kept.push(worker);
                }
            }
            *workers = kept;
            ready
        };
        for handle in finished {
            let _ = handle.join();
        }
        let matching = lock(&inner.workers)
            .iter()
            .filter(|worker| worker.kind == kind)
            .count();
        if matching == 0 {
            return;
        }
        if Instant::now() >= deadline {
            let mut workers = lock(&inner.workers);
            workers.retain(|worker| worker.kind != kind);
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn join_requests(inner: &Arc<Inner>, deadline: Instant) {
    loop {
        let ready = {
            let mut requests = lock(&inner.requests);
            let mut ready = Vec::new();
            let mut kept = Vec::with_capacity(requests.len());
            for handle in requests.drain(..) {
                if handle.is_finished() {
                    ready.push(handle);
                } else {
                    kept.push(handle);
                }
            }
            *requests = kept;
            ready
        };
        for handle in ready {
            let _ = handle.join();
        }
        if lock(&inner.requests).is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            lock(&inner.requests).clear();
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn ensure_entry(
    inner: &Arc<Inner>,
    entries: &mut HashMap<String, Arc<Entry>>,
    song: &Song,
) -> Arc<Entry> {
    if let Some(entry) = entries.get(&song.id) {
        return entry.clone();
    }
    let digest = Sha256::digest(song.id.as_bytes());
    let cache_path = inner.temp_path.join(format!("{}.part", hex(&digest)));
    let entry = Arc::new(Entry {
        id: song.id.clone(),
        cache_path,
        fetch_lock: Mutex::new(()),
        state: Mutex::new(EntryState {
            song: song.clone(),
            total: None,
            ranges: Vec::new(),
            content_type: String::new(),
            extension: String::new(),
            disposition: String::new(),
            playback: None,
            manual_retained: false,
            manual_job: None,
            manual_workers: 0,
            active_handlers: 0,
            discard_requested: false,
            state: IDLE,
            last_error: String::new(),
            saving: false,
            saved: false,
            save_attempted: false,
        }),
    });
    entries.insert(song.id.clone(), entry.clone());
    entry
}

fn run_manual(inner: Arc<Inner>, entry: Arc<Entry>, job: Arc<ManualJob>) {
    if !inner.manual_gate.acquire(&job.cancel) {
        finish_manual(&inner, &entry, &job, Ok(()));
        return;
    }

    let _gate = ManualGateGuard(&inner.manual_gate);
    if job.cancel.is_canceled() || inner.closed.load(Ordering::Acquire) {
        finish_manual(&inner, &entry, &job, Ok(()));
        return;
    }
    {
        let mut state = lock(&entry.state);
        if state
            .manual_job
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &job))
        {
            state.state = DOWNLOADING;
        }
    }

    let result = download_all(&inner, &entry, &job.source_url, &job.cancel);
    finish_manual(&inner, &entry, &job, result);
}

struct ManualGateGuard<'a>(&'a ManualGate);

impl Drop for ManualGateGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn finish_manual(inner: &Arc<Inner>, entry: &Arc<Entry>, job: &Arc<ManualJob>, result: Result<()>) {
    let canceled = job.cancel.is_canceled() || inner.closed.load(Ordering::Acquire);
    job.cancel.cancel();
    {
        let mut state = lock(&entry.state);
        let current = state
            .manual_job
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, job));
        if current {
            state.manual_job = None;
        }
        state.manual_workers = state.manual_workers.saturating_sub(1);
        if current {
            match result {
                _ if canceled => {
                    state.state = if state.manual_retained {
                        QUEUED
                    } else {
                        CANCELED
                    };
                }
                Err(error) => {
                    state.last_error = sanitize_error(&format!("{error:#}"));
                    state.state = ERROR;
                }
                Ok(()) if state.is_full() => {
                    state.state = SAVING;
                }
                Ok(()) => {
                    state.state = IDLE;
                }
            }
        }
    }
    maybe_save(inner, entry);
    reconcile(inner, entry);
}

fn download_all(
    inner: &Arc<Inner>,
    entry: &Arc<Entry>,
    source_url: &str,
    cancel: &CancelToken,
) -> Result<()> {
    let mut start = 0u64;
    loop {
        if canceled(inner, cancel) {
            bail!("canceled");
        }
        let (total, ranges) = {
            let state = lock(&entry.state);
            (state.total, state.ranges.clone())
        };
        if total.is_some_and(|total| start >= total) {
            return Ok(());
        }

        let mut end = start.saturating_add(MANUAL_CHUNK_SIZE);
        if let Some(total) = total {
            end = end.min(total);
        }
        if end <= start {
            return Ok(());
        }

        if let Some((hole_start, hole_end)) = missing_chunk(&ranges, start, end - 1) {
            let result =
                match fetch_chunk(inner, entry, source_url, hole_start, hole_end + 1, cancel) {
                    Ok(result) => result,
                    Err(error) => {
                        if let Some(range_error) = error.downcast_ref::<RangeNotSatisfiable>() {
                            let new_total = range_error.total.or(total);
                            if new_total.is_some_and(|total| hole_start >= total) {
                                return Ok(());
                            }
                        }
                        return Err(error);
                    }
                };
            if result.end <= hole_start {
                let state = lock(&entry.state);
                if result.total.is_some_and(|total| hole_start >= total)
                    || state.total.is_some_and(|total| hole_start >= total)
                {
                    return Ok(());
                }
                bail!("source made no progress");
            }
            start = result.end;
            if result.total.is_some_and(|total| start >= total) {
                return Ok(());
            }
            continue;
        }

        if total.is_none() {
            bail!("source length is unavailable");
        }
        start = end;
    }
}

fn fetch_chunk(
    inner: &Arc<Inner>,
    entry: &Arc<Entry>,
    source_url: &str,
    start: u64,
    requested_end: u64,
    cancel: &CancelToken,
) -> Result<FetchResult> {
    if canceled(inner, cancel) {
        bail!("canceled");
    }
    let _fetch = lock(&entry.fetch_lock);
    if canceled(inner, cancel) {
        bail!("canceled");
    }

    let requested_end = requested_end.max(start.saturating_add(1));
    let total = lock(&entry.state).total;
    if total.is_some_and(|total| start >= total) {
        return Err(RangeNotSatisfiable { total }.into());
    }
    let requested_end = total.map_or(requested_end, |total| requested_end.min(total));
    {
        let state = lock(&entry.state);
        if contains_range(&state.ranges, start, requested_end - 1) {
            return Ok(FetchResult {
                end: requested_end,
                total: state.total,
                content_type: state.content_type.clone(),
                extension: state.extension.clone(),
                disposition: state.disposition.clone(),
            });
        }
    }

    let mut response = inner
        .client
        .get(source_url)
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start, requested_end - 1),
        )
        .timeout(CHUNK_TIMEOUT)
        .send()
        .map_err(|error| {
            if canceled(inner, cancel) {
                anyhow!("canceled")
            } else {
                anyhow!(
                    "source request failed: {}",
                    sanitize_error(&error.to_string())
                )
            }
        })?;

    let content_type = validate_content_type(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    )?;
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut result = FetchResult {
        end: start,
        total: None,
        content_type,
        extension: extension_from_url(source_url),
        disposition,
    };
    let mut allows_unknown_eof = false;
    let (read_start, max_length) = match response.status().as_u16() {
        206 => {
            let range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("source returned an invalid byte range"))?;
            let range = parse_content_range(range)
                .map_err(|_| anyhow!("source returned an invalid byte range"))?;
            let (Some(range_start), Some(range_end)) = (range.start, range.end) else {
                bail!("source returned an invalid byte range");
            };
            if range_start != start || range_end < range_start {
                bail!("source returned an invalid byte range");
            }
            result.total = range.total;
            let length = range_end - range_start + 1;
            let wanted = requested_end - start;
            (range_start, length.min(wanted))
        }
        200 => {
            result.total = response.content_length();
            allows_unknown_eof = result.total.is_none();
            if result.total.is_some_and(|total| start >= total) {
                return Err(RangeNotSatisfiable {
                    total: result.total,
                }
                .into());
            }
            let wanted = requested_end - start;
            let length = result
                .total
                .map_or(wanted, |total| wanted.min(total.saturating_sub(start)));
            (0, length)
        }
        416 => {
            let total = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_content_range(value).ok())
                .and_then(|range| range.total);
            drain_response(&mut response);
            if let Some(total) = total {
                set_total(&entry.state, total)?;
            }
            return Err(RangeNotSatisfiable { total }.into());
        }
        status => {
            drain_response(&mut response);
            bail!("source returned HTTP {status}");
        }
    };

    if let Some(total) = result.total {
        set_total(&entry.state, total)?;
    }
    {
        let state = lock(&entry.state);
        result.total = state.total;
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&entry.cache_path)
        .context("open playback cache")?;
    if let Some(total) = result.total {
        file.set_len(total).context("resize playback cache")?;
    }

    if max_length > 0 && read_start < start {
        skip_response(&mut response, start - read_start, cancel, inner)?;
    }

    let mut offset = start;
    let target = start.saturating_add(max_length);
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut interrupted = None;
    let mut clean_eof = false;
    while offset < target {
        if canceled(inner, cancel) {
            interrupted = Some(anyhow!("canceled"));
            break;
        }
        let remaining =
            usize::try_from((target - offset).min(buffer.len() as u64)).unwrap_or(buffer.len());
        match response.read(&mut buffer[..remaining]) {
            Ok(0) => {
                clean_eof = true;
                break;
            }
            Ok(read) => {
                let mut written = 0;
                while written < read {
                    let count = file
                        .write_at(&buffer[written..read], offset + written as u64)
                        .context("write playback cache")?;
                    if count == 0 {
                        bail!("write playback cache made no progress");
                    }
                    written += count;
                }
                let chunk_start = offset;
                offset += written as u64;
                let mut state = lock(&entry.state);
                if offset > chunk_start {
                    state.ranges = add_coverage(state.ranges.clone(), chunk_start, offset - 1);
                    state.last_error.clear();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                interrupted = Some(anyhow!(
                    "source stream interrupted: {}",
                    sanitize_error(&error.to_string())
                ));
                break;
            }
        }
    }

    if interrupted.is_none()
        && clean_eof
        && allows_unknown_eof
        && result.total.is_none()
        && start.saturating_add(max_length) > offset
    {
        let inferred_total = offset;
        result.total = Some(inferred_total);
        set_total(&entry.state, inferred_total)?;
    }
    if result.total == Some(offset) {
        file.sync_all().context("sync playback cache")?;
    }
    result.end = offset;
    {
        let mut state = lock(&entry.state);
        if state.content_type.is_empty() && !result.content_type.is_empty() {
            state.content_type = result.content_type.clone();
        }
        if state.extension.is_empty() && !result.extension.is_empty() {
            state.extension = result.extension.clone();
        }
        if state.disposition.is_empty() && !result.disposition.is_empty() {
            state.disposition = result.disposition.clone();
        }
        result.total = state.total;
    }

    if let Some(error) = interrupted {
        return Err(error);
    }
    let expected = start.saturating_add(max_length);
    if max_length > 0
        && offset < expected
        && !(interrupted.is_none() && clean_eof && allows_unknown_eof)
    {
        bail!("source stream interrupted");
    }
    Ok(result)
}

fn skip_response(
    response: &mut Response,
    amount: u64,
    cancel: &CancelToken,
    inner: &Arc<Inner>,
) -> Result<()> {
    let mut remaining = amount;
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    while remaining > 0 {
        if canceled(inner, cancel) {
            bail!("canceled");
        }
        let read_size = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = response
            .read(&mut buffer[..read_size])
            .context("source stream interrupted")?;
        if read == 0 {
            bail!("source stream interrupted");
        }
        remaining -= read as u64;
    }
    Ok(())
}

fn drain_response(response: &mut Response) {
    let mut sink = [0u8; 4096];
    let _ = response.take(sink.len() as u64).read(&mut sink);
}

fn serve_request(inner: Arc<Inner>, request: Request) {
    let parsed = Url::parse(&format!("http://loopback{}", request.url()));
    let Ok(url) = parsed else {
        let _ = request.respond(HttpResponse::empty(StatusCode(404)));
        return;
    };
    if url.path() != "/stream" {
        let _ = request.respond(HttpResponse::empty(StatusCode(404)));
        return;
    }
    if !matches!(request.method(), Method::Get | Method::Head) {
        let response = HttpResponse::from_string("method not allowed")
            .with_status_code(StatusCode(405))
            .with_header(static_header("Allow", "GET, HEAD"));
        let _ = request.respond(response);
        return;
    }

    let mut song_id = None;
    let mut token = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "id" => song_id = Some(value.into_owned()),
            "token" => token = Some(value.into_owned()),
            _ => {}
        }
    }
    let (Some(song_id), Some(token)) = (song_id, token) else {
        let _ = request.respond(HttpResponse::empty(StatusCode(404)));
        return;
    };

    let entry = lock(&inner.entries).get(&song_id).cloned();
    let Some(entry) = entry else {
        let _ = request.respond(HttpResponse::empty(StatusCode(404)));
        return;
    };
    let run = {
        let mut state = lock(&entry.state);
        if inner.closed.load(Ordering::Acquire) {
            let _ = request.respond(HttpResponse::empty(StatusCode(404)));
            return;
        }
        let Some(run) = state
            .playback
            .as_ref()
            .filter(|run| run.token == token)
            .cloned()
        else {
            let _ = request.respond(HttpResponse::empty(StatusCode(404)));
            return;
        };
        state.active_handlers += 1;
        state.discard_requested = false;
        run
    };
    let guard = HandlerGuard {
        inner: inner.clone(),
        entry: entry.clone(),
    };

    let range_header = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Range"))
        .map(|header| header.value.as_str());
    let range = match parse_client_range(range_header) {
        Ok(range) => range,
        Err(_) => {
            let mut response =
                HttpResponse::from_string("invalid byte range").with_status_code(StatusCode(416));
            response.add_header(static_header("Content-Range", "bytes */*"));
            let _ = request.respond(response);
            return;
        }
    };

    let prepared = match prepare_playback(&inner, &entry, &run.source_url, &run.cancel, &range) {
        Ok(prepared) => prepared,
        Err(PlaybackError::Range(total)) => {
            let content_range = total
                .map(|total| format!("bytes */{total}"))
                .unwrap_or_else(|| "bytes */*".to_owned());
            let mut response = HttpResponse::from_string("range not satisfiable")
                .with_status_code(StatusCode(416));
            response.add_header(dynamic_header("Content-Range", &content_range));
            let _ = request.respond(response);
            return;
        }
        Err(PlaybackError::Source(error)) => {
            if !canceled(&inner, &run.cancel) {
                record_error(&entry, &error);
            }
            let _ = request.respond(
                HttpResponse::from_string("source transfer failed")
                    .with_status_code(StatusCode(502)),
            );
            return;
        }
    };

    let mut headers = vec![
        static_header("Accept-Ranges", "bytes"),
        dynamic_header(
            "Content-Type",
            if prepared.content_type.is_empty() {
                "application/octet-stream"
            } else {
                &prepared.content_type
            },
        ),
    ];
    if let Some(length) = prepared.length {
        headers.push(dynamic_header("Content-Length", &length.to_string()));
    }
    if let Some((start, end, total)) = prepared.content_range {
        let value = total.map_or_else(
            || format!("bytes {start}-{end}/*"),
            |total| format!("bytes {start}-{end}/{total}"),
        );
        headers.push(dynamic_header("Content-Range", &value));
    }

    if matches!(request.method(), Method::Head) {
        let mut response = HttpResponse::empty(prepared.status);
        for header in headers {
            response.add_header(header);
        }
        let _ = request.respond(response);
        drop(guard);
        return;
    }

    let file = match File::open(&entry.cache_path) {
        Ok(file) => file,
        Err(error) => {
            record_error(&entry, &anyhow!("open playback cache: {error}"));
            let _ = request.respond(
                HttpResponse::from_string("cache read failed").with_status_code(StatusCode(502)),
            );
            return;
        }
    };
    let reader = PlaybackReader {
        inner: inner.clone(),
        entry: entry.clone(),
        run,
        file,
        position: prepared.start,
        end: prepared.end,
    };
    let response = HttpResponse::new(
        prepared.status,
        headers,
        reader,
        prepared
            .length
            .and_then(|length| usize::try_from(length).ok()),
        None,
    );
    let _ = request.respond(response);
    drop(guard);
}

fn handle_request(inner: Arc<Inner>, request: Request) {
    serve_request(inner, request);
}

#[derive(Debug)]
enum PlaybackError {
    Range(Option<u64>),
    Source(anyhow::Error),
}

struct PreparedPlayback {
    status: StatusCode,
    start: u64,
    end: Option<u64>,
    length: Option<u64>,
    content_type: String,
    content_range: Option<(u64, u64, Option<u64>)>,
}

struct PlaybackReader {
    inner: Arc<Inner>,
    entry: Arc<Entry>,
    run: Arc<PlaybackRun>,
    file: File,
    position: u64,
    end: Option<u64>,
}

impl Read for PlaybackReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.end.is_some_and(|end| self.position >= end) {
            return Ok(0);
        }
        if canceled(&self.inner, &self.run.cancel) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
        }

        let wanted = self
            .end
            .map_or(buffer.len() as u64, |end| {
                (buffer.len() as u64).min(end.saturating_sub(self.position))
            })
            .min(buffer.len() as u64) as usize;
        let target = self.position.saturating_add(wanted as u64);
        let mut filled = 0usize;

        while self.position < target {
            let state = lock(&self.entry.state);
            let (total, ranges) = (state.total, state.ranges.clone());
            drop(state);
            if total.is_some_and(|total| self.position >= total) {
                self.end = total;
                break;
            }
            let chunk_end = total
                .map_or(self.position + PLAYBACK_CHUNK_SIZE, |total| {
                    total.min(self.position + PLAYBACK_CHUNK_SIZE)
                })
                .min(target);
            if chunk_end <= self.position {
                break;
            }
            if let Some((hole_start, hole_end)) =
                missing_chunk(&ranges, self.position, chunk_end - 1)
            {
                let end = hole_end + 1;
                let result = fetch_chunk(
                    &self.inner,
                    &self.entry,
                    &self.run.source_url,
                    hole_start,
                    end,
                    &self.run.cancel,
                )
                .map_err(|error| io::Error::other(sanitize_error(&format!("{error:#}"))))?;
                if result.end <= hole_start {
                    self.end = Some(self.position);
                    break;
                }
                continue;
            }

            self.file
                .seek(SeekFrom::Start(self.position))
                .map_err(io::Error::other)?;
            let take = usize::try_from(target - self.position).unwrap_or(buffer.len());
            match self.file.read(&mut buffer[filled..filled + take]) {
                Ok(0) => {
                    if let Some(total) = total {
                        self.end = Some(total);
                    } else {
                        let error = anyhow!("read playback cache returned no data");
                        record_error(&self.entry, &error);
                        return Err(io::Error::other(sanitize_error(&format!("{error:#}"))));
                    }
                    break;
                }
                Ok(read) => {
                    filled += read;
                    self.position += read as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    record_error(
                        &self.entry,
                        &anyhow!(
                            "read playback cache: {}",
                            sanitize_error(&error.to_string())
                        ),
                    );
                    return Err(error);
                }
            }
        }
        Ok(filled)
    }
}

struct HandlerGuard {
    inner: Arc<Inner>,
    entry: Arc<Entry>,
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.entry.state);
            state.active_handlers = state.active_handlers.saturating_sub(1);
        }
        reconcile(&self.inner, &self.entry);
    }
}

fn prepare_playback(
    inner: &Arc<Inner>,
    entry: &Arc<Entry>,
    source_url: &str,
    cancel: &CancelToken,
    requested: &ClientRange,
) -> std::result::Result<PreparedPlayback, PlaybackError> {
    let mut total = lock(&entry.state).total;
    let need_probe = total.is_none()
        || matches!(
            requested,
            ClientRange::Bounded { start, .. }
                if !contains_range(&lock(&entry.state).ranges, *start, *start)
        );

    if need_probe {
        let (probe_start, probe_end) = match requested {
            ClientRange::Bounded { start, end } => (
                *start,
                end.map_or(start.saturating_add(PLAYBACK_CHUNK_SIZE), |end| {
                    start.saturating_add(1).min(end.saturating_add(1))
                }),
            ),
            ClientRange::Suffix(_) | ClientRange::Full => (0, PLAYBACK_CHUNK_SIZE),
        };
        match fetch_chunk(inner, entry, source_url, probe_start, probe_end, cancel) {
            Ok(_) => {}
            Err(error) => {
                if let Some(range_error) = error.downcast_ref::<RangeNotSatisfiable>() {
                    return Err(PlaybackError::Range(range_error.total));
                }
                return Err(PlaybackError::Source(error));
            }
        }
        total = lock(&entry.state).total;
    }

    let (ranged, start, end, status) = match requested {
        ClientRange::Full => (false, 0, total, StatusCode(200)),
        ClientRange::Bounded { start, end } => {
            if total.is_some_and(|total| *start >= total) {
                return Err(PlaybackError::Range(total));
            }
            let end = match (end, total) {
                (Some(end), Some(total)) => Some((*end).saturating_add(1).min(total)),
                (Some(end), None) => Some((*end).saturating_add(1)),
                (None, total) => total,
            };
            if end.is_some_and(|end| end <= *start) {
                return Err(PlaybackError::Range(total));
            }
            (true, *start, end, StatusCode(206))
        }
        ClientRange::Suffix(suffix) => {
            let Some(total) = total else {
                return Err(PlaybackError::Range(None));
            };
            if total == 0 {
                return Err(PlaybackError::Range(Some(total)));
            }
            (
                true,
                total.saturating_sub(*suffix),
                Some(total),
                StatusCode(206),
            )
        }
    };

    let length = end.map(|end| end.saturating_sub(start));
    let content_type = lock(&entry.state).content_type.clone();
    let content_range = if ranged {
        let Some(end) = end else {
            return Err(PlaybackError::Range(total));
        };
        Some((start, end.saturating_sub(1), total))
    } else {
        None
    };
    Ok(PreparedPlayback {
        status,
        start,
        end,
        length,
        content_type,
        content_range,
    })
}

fn maybe_save(inner: &Arc<Inner>, entry: &Arc<Entry>) {
    let snapshot = {
        let entries = lock(&inner.entries);
        if inner.closed.load(Ordering::Acquire)
            || !entries
                .get(&entry.id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            return;
        }
        let mut state = lock(&entry.state);
        if !state.is_full() || state.saved || state.saving || state.save_attempted {
            return;
        }
        state.saving = true;
        state.save_attempted = true;
        state.state = SAVING;
        (
            state.song.clone(),
            entry.cache_path.clone(),
            choose_extension(&state.content_type, &state.extension, &state.disposition),
        )
    };

    let inner_for_save = inner.clone();
    let entry_for_save = entry.clone();
    let (song, path, extension) = snapshot;
    let result = spawn_worker(inner, "akari-transfer-save", WorkerKind::Save, move || {
        let result = inner_for_save.lib.save_download(&song, &path, &extension);
        {
            let mut state = lock(&entry_for_save.state);
            state.saving = false;
            match result {
                Ok(()) => {
                    state.saved = true;
                    state.last_error.clear();
                    state.state = COMPLETE;
                }
                Err(error) => {
                    state.last_error = sanitize_error(&format!("save download failed: {error:#}"));
                    state.state = ERROR;
                }
            }
        }
        reconcile(&inner_for_save, &entry_for_save);
    });
    if let Err(error) = result {
        let mut state = lock(&entry.state);
        state.saving = false;
        state.last_error = sanitize_error(&format!("start save worker: {error}"));
        state.state = ERROR;
    }
}

fn reconcile(inner: &Arc<Inner>, entry: &Arc<Entry>) {
    let (remove, full, saved, save_attempted) = {
        let state = lock(&entry.state);
        let idle = state.active_handlers == 0 && state.manual_workers == 0 && !state.saving;
        let has_playback = state
            .playback
            .as_ref()
            .is_some_and(|playback| playback.leases.load(Ordering::Acquire) > 0);
        (
            state.discard_requested
                && !state.manual_retained
                && !has_playback
                && idle
                && (!state.is_full() || state.saved || state.save_attempted),
            state.is_full(),
            state.saved,
            state.save_attempted,
        )
    };
    if full && !saved && !save_attempted {
        maybe_save(inner, entry);
        return;
    }
    if remove {
        remove_entry(inner, entry);
    }
}

fn remove_entry(inner: &Arc<Inner>, entry: &Arc<Entry>) {
    let can_remove = {
        let mut entries = lock(&inner.entries);
        let state = lock(&entry.state);
        let idle = state.active_handlers == 0 && state.manual_workers == 0 && !state.saving;
        let no_playback = state
            .playback
            .as_ref()
            .is_none_or(|playback| playback.leases.load(Ordering::Acquire) == 0);
        if !inner.closed.load(Ordering::Acquire)
            && idle
            && state.discard_requested
            && !state.manual_retained
            && no_playback
            && entries
                .get(&entry.id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&entry.id);
            true
        } else {
            false
        }
    };
    if can_remove {
        let _ = fs::remove_file(&entry.cache_path);
    }
}

fn record_error(entry: &Arc<Entry>, error: &anyhow::Error) {
    let mut state = lock(&entry.state);
    state.last_error = sanitize_error(&format!("{error:#}"));
    state.state = ERROR;
}

fn canceled(inner: &Inner, cancel: &CancelToken) -> bool {
    inner.closed.load(Ordering::Acquire) || cancel.is_canceled()
}

impl EntryState {
    fn is_full(&self) -> bool {
        self.total
            .is_some_and(|total| total > 0 && covered_bytes(&self.ranges) == total)
    }

    fn status(&self) -> Status {
        let state = if !self.last_error.is_empty() {
            ERROR
        } else if self.saving {
            SAVING
        } else if self.manual_job.is_some() && self.state == QUEUED {
            QUEUED
        } else if self.manual_job.is_some() && self.state == DOWNLOADING {
            DOWNLOADING
        } else if self.active_handlers > 0 {
            STREAMING
        } else if self.saved || self.is_full() {
            COMPLETE
        } else if self.state == CANCELED {
            CANCELED
        } else {
            IDLE
        };
        Status {
            song: self.song.clone(),
            state: state.to_owned(),
            bytes: covered_bytes(&self.ranges),
            total: self.total.unwrap_or_default(),
            error: self.last_error.clone(),
        }
    }
}

fn add_coverage(mut ranges: Vec<ByteRange>, start: u64, end: u64) -> Vec<ByteRange> {
    if end < start {
        return ranges;
    }
    let mut next = ByteRange { start, end };
    let mut result = Vec::with_capacity(ranges.len() + 1);
    let mut inserted = false;
    for current in ranges.drain(..) {
        if current.end.saturating_add(1) < next.start {
            result.push(current);
            continue;
        }
        if next.end.saturating_add(1) < current.start {
            if !inserted {
                result.push(next);
                inserted = true;
            }
            result.push(current);
            continue;
        }
        next.start = next.start.min(current.start);
        next.end = next.end.max(current.end);
    }
    if !inserted {
        result.push(next);
    }
    result
}

fn covered_bytes(ranges: &[ByteRange]) -> u64 {
    ranges
        .iter()
        .map(|range| range.end.saturating_sub(range.start).saturating_add(1))
        .sum()
}

fn contains_range(ranges: &[ByteRange], start: u64, end: u64) -> bool {
    end >= start
        && ranges
            .iter()
            .any(|range| range.start <= start && range.end >= end)
}

fn missing_chunk(ranges: &[ByteRange], start: u64, end: u64) -> Option<(u64, u64)> {
    if end < start {
        return None;
    }
    let mut cursor = start;
    for range in ranges {
        if range.end < cursor {
            continue;
        }
        if range.start > cursor {
            return Some((cursor, range.start.saturating_sub(1).min(end)));
        }
        if range.end >= end {
            return None;
        }
        cursor = range.end.saturating_add(1);
    }
    (cursor <= end).then_some((cursor, end))
}

fn parse_client_range(value: Option<&str>) -> Result<ClientRange> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ClientRange::Full);
    };
    if value.contains(',') {
        bail!("multiple ranges");
    }
    let (unit, spec) = value
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid range unit"))?;
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        bail!("invalid range unit");
    }
    let (first, last) = spec
        .trim()
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid range"))?;
    let first = first.trim();
    let last = last.trim();
    if first.is_empty() {
        let suffix = last.parse::<u64>().context("invalid suffix range")?;
        if suffix == 0 {
            bail!("invalid suffix range");
        }
        return Ok(ClientRange::Suffix(suffix));
    }
    let start = first.parse::<u64>().context("invalid range start")?;
    let end = if last.is_empty() {
        None
    } else {
        let end = last.parse::<u64>().context("invalid range end")?;
        if end < start {
            bail!("invalid range end");
        }
        Some(end)
    };
    Ok(ClientRange::Bounded { start, end })
}

fn parse_content_range(value: &str) -> Result<ContentRange> {
    let (unit, value) = value
        .trim()
        .split_once(' ')
        .ok_or_else(|| anyhow!("invalid content range"))?;
    if !unit.eq_ignore_ascii_case("bytes") {
        bail!("invalid content range");
    }
    let (span, total) = value
        .trim()
        .split_once('/')
        .ok_or_else(|| anyhow!("invalid content range"))?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(
            total
                .trim()
                .parse::<u64>()
                .context("invalid content range total")?,
        )
    };
    if span.trim() == "*" {
        return Ok(ContentRange {
            start: None,
            end: None,
            total,
        });
    }
    let (start, end) = span
        .trim()
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid content range span"))?;
    let start = start
        .trim()
        .parse::<u64>()
        .context("invalid content range start")?;
    let end = end
        .trim()
        .parse::<u64>()
        .context("invalid content range end")?;
    if end < start || total.is_some_and(|total| end >= total) {
        bail!("invalid content range");
    }
    Ok(ContentRange {
        start: Some(start),
        end: Some(end),
        total,
    })
}

fn set_total(state: &Mutex<EntryState>, total: u64) -> Result<()> {
    let mut state = lock(state);
    if let Some(known) = state.total
        && known != total
    {
        bail!("source length changed");
    }
    state.total = Some(total);
    Ok(())
}

fn validate_content_type(value: Option<&str>) -> Result<String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(String::new());
    };
    let media_type = value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase();
    if media_type.is_empty()
        || !media_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
    {
        bail!("source returned an invalid content type");
    }
    if media_type == "application/json"
        || media_type == "application/xml"
        || media_type == "text/xml"
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
    {
        bail!("source returned a JSON or XML error");
    }
    Ok(media_type)
}

fn choose_extension(content_type: &str, original: &str, disposition: &str) -> String {
    let from_content_type = match content_type.to_ascii_lowercase().as_str() {
        "audio/mpeg" | "audio/mp3" => Some(".mp3"),
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => Some(".m4a"),
        "audio/aac" | "audio/aacp" => Some(".aac"),
        "audio/flac" | "audio/x-flac" => Some(".flac"),
        "audio/ogg" | "application/ogg" => Some(".ogg"),
        "audio/opus" => Some(".opus"),
        "audio/wav" | "audio/wave" | "audio/x-wav" => Some(".wav"),
        "audio/webm" => Some(".weba"),
        "audio/x-ms-wma" => Some(".wma"),
        "audio/ape" | "audio/x-ape" => Some(".ape"),
        _ => None,
    };
    from_content_type
        .or_else(|| disposition_extension(disposition))
        .or_else(|| supported_audio_extension(original))
        .unwrap_or(".bin")
        .to_owned()
}

fn disposition_extension(value: &str) -> Option<&'static str> {
    let filename = value.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("filename=")
            .map(|value| value.trim().trim_matches('"'))
    })?;
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| supported_audio_extension(&format!(".{extension}")))
}

fn extension_from_url(value: &str) -> String {
    Url::parse(value)
        .ok()
        .and_then(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| supported_audio_extension(&format!(".{extension}")))
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn supported_audio_extension(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        ".aac" => Some(".aac"),
        ".aif" => Some(".aif"),
        ".aiff" => Some(".aiff"),
        ".alac" => Some(".alac"),
        ".ape" => Some(".ape"),
        ".flac" => Some(".flac"),
        ".m4a" => Some(".m4a"),
        ".mp3" => Some(".mp3"),
        ".mp4" => Some(".mp4"),
        ".oga" => Some(".oga"),
        ".ogg" => Some(".ogg"),
        ".opus" => Some(".opus"),
        ".wav" => Some(".wav"),
        ".weba" => Some(".weba"),
        ".webm" => Some(".webm"),
        ".wma" => Some(".wma"),
        _ => None,
    }
}

fn raw_source_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value).context("invalid source URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("invalid source URL");
    }
    let mut pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "format")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.push(("format".into(), "raw".into()));
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url.into())
}

fn random_token() -> String {
    hex(&random::<[u8; 16]>())
}

fn hex(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn sanitize_error(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn static_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static HTTP header")
}

fn dynamic_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid dynamic HTTP header")
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Cursor, Read, Write},
        net::{Shutdown, TcpStream},
    };
    use tempfile::TempDir;

    fn song(id: &str) -> Song {
        Song {
            id: id.into(),
            title: "Track".into(),
            artist: "Artist".into(),
            album_id: "album".into(),
            album_title: "Album".into(),
            duration: 120.0,
            track_number: 1,
            disc_number: 1,
            artwork_url: String::new(),
        }
    }

    #[derive(Clone)]
    struct SaveRecord {
        path: PathBuf,
        extension: String,
    }

    struct MockBackend {
        _dir: TempDir,
        stream: String,
        download: String,
        client: Client,
        local: Mutex<HashMap<String, PathBuf>>,
        saves: Mutex<Vec<SaveRecord>>,
    }

    impl MockBackend {
        fn new(source: &MockServer) -> Self {
            let dir = tempfile::tempdir().expect("mock library directory");
            Self {
                _dir: dir,
                stream: source.url.clone(),
                download: source.url.clone(),
                client: Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .expect("mock client"),
                local: Mutex::new(HashMap::new()),
                saves: Mutex::new(Vec::new()),
            }
        }
    }

    impl LibraryBackend for MockBackend {
        fn local_path(&self, song_id: &str) -> Option<PathBuf> {
            lock(&self.local).get(song_id).cloned()
        }

        fn stream_url(&self, _song_id: &str) -> Result<String> {
            Ok(self.stream.clone())
        }

        fn download_url(&self, _song_id: &str) -> Result<String> {
            Ok(self.download.clone())
        }

        fn http_client(&self) -> Client {
            self.client.clone()
        }

        fn save_download(&self, song: &Song, temp: &Path, extension: &str) -> Result<()> {
            let destination = self._dir.path().join(format!("{}{}", song.id, extension));
            fs::copy(temp, &destination).context("copy mock download")?;
            lock(&self.local).insert(song.id.clone(), destination.clone());
            lock(&self.saves).push(SaveRecord {
                path: destination,
                extension: extension.to_owned(),
            });
            Ok(())
        }
    }

    struct MockServer {
        url: String,
        _server: Arc<Server>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(Request) + Send + Sync + 'static,
        {
            let server = Arc::new(Server::http("127.0.0.1:0").expect("mock source server"));
            let address = server.server_addr().to_ip().expect("mock source address");
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread_server = server.clone();
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match thread_server.recv_timeout(Duration::from_millis(20)) {
                        Ok(Some(request)) => handler(request),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{address}/download/test.mp3?format=raw"),
                _server: server,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn manager_for(backend: Arc<MockBackend>) -> Manager {
        Manager::with_backend(backend).expect("transfer manager")
    }

    fn request_range(request: &Request) -> Option<(u64, u64)> {
        let header = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Range"))
            .map(|header| header.value.as_str());
        match parse_client_range(header).expect("valid source range") {
            ClientRange::Bounded { start, end } => Some((start, end.unwrap_or(start))),
            ClientRange::Full | ClientRange::Suffix(_) => None,
        }
    }

    fn respond_ranged(request: Request, body: Vec<u8>) {
        let total = body.len() as u64;
        if let Some((requested_start, requested_end)) = request_range(&request) {
            let start = requested_start.min(total);
            let end = requested_end.saturating_add(1).min(total);
            if start >= total || end <= start {
                let mut response = HttpResponse::from_string("range not satisfiable")
                    .with_status_code(StatusCode(416));
                response.add_header(dynamic_header("Content-Range", &format!("bytes */{total}")));
                let _ = request.respond(response);
                return;
            }
            let chunk = body[start as usize..end as usize].to_vec();
            let response = HttpResponse::from_data(chunk)
                .with_status_code(StatusCode(206))
                .with_header(static_header("Content-Type", "audio/mpeg"))
                .with_header(dynamic_header(
                    "Content-Range",
                    &format!("bytes {start}-{}/{total}", end - 1),
                ))
                .with_header(static_header("Accept-Ranges", "bytes"));
            let _ = request.respond(response);
        } else {
            let response = HttpResponse::from_data(body)
                .with_status_code(StatusCode(200))
                .with_header(static_header("Content-Type", "audio/mpeg"))
                .with_header(static_header("Accept-Ranges", "bytes"));
            let _ = request.respond(response);
        }
    }

    fn respond_chunked(request: Request, body: Vec<u8>) {
        let headers = vec![
            static_header("Content-Type", "audio/mpeg"),
            static_header("Accept-Ranges", "bytes"),
        ];
        let response = HttpResponse::new(StatusCode(200), headers, Cursor::new(body), None, None);
        let _ = request.respond(response);
    }

    struct SlowBody {
        data: Cursor<Vec<u8>>,
        delay: Duration,
    }

    impl Read for SlowBody {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            thread::sleep(self.delay);
            let limit = buffer.len().min(4 << 10);
            self.data.read(&mut buffer[..limit])
        }
    }

    fn respond_slow_range(request: Request, body: Vec<u8>, delay: Duration) {
        let total = body.len() as u64;
        let (start, end) = request_range(&request).unwrap_or((0, total - 1));
        let end = end.saturating_add(1).min(total);
        let data = body[start as usize..end as usize].to_vec();
        let length = data.len();
        let headers = vec![
            static_header("Content-Type", "audio/mpeg"),
            dynamic_header(
                "Content-Range",
                &format!("bytes {start}-{}/{total}", end - 1),
            ),
        ];
        let response = HttpResponse::new(
            StatusCode(206),
            headers,
            SlowBody {
                data: Cursor::new(data),
                delay,
            },
            Some(length),
            None,
        );
        let _ = request.respond(response);
    }

    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn coverage_merges_ranges_and_finds_holes() {
        let ranges = add_coverage(Vec::new(), 10, 19);
        let ranges = add_coverage(ranges, 20, 29);
        let ranges = add_coverage(ranges, 0, 4);
        let ranges = add_coverage(ranges, 3, 12);
        assert_eq!(ranges, vec![ByteRange { start: 0, end: 29 }]);
        assert_eq!(covered_bytes(&ranges), 30);
        assert!(contains_range(&ranges, 0, 29));
        assert!(!contains_range(&ranges, 0, 30));
        assert_eq!(missing_chunk(&ranges, 0, 40), Some((30, 40)));

        let split = add_coverage(vec![ByteRange { start: 8, end: 12 }], 0, 3);
        assert_eq!(
            split,
            vec![
                ByteRange { start: 0, end: 3 },
                ByteRange { start: 8, end: 12 },
            ]
        );
        assert_eq!(missing_chunk(&split, 0, 12), Some((4, 7)));
    }

    #[test]
    fn parses_http_ranges_strictly() {
        assert!(matches!(
            parse_client_range(None).unwrap(),
            ClientRange::Full
        ));
        assert!(matches!(
            parse_client_range(Some("bytes=5-9")).unwrap(),
            ClientRange::Bounded {
                start: 5,
                end: Some(9)
            }
        ));
        assert!(matches!(
            parse_client_range(Some("bytes=-8")).unwrap(),
            ClientRange::Suffix(8)
        ));
        assert!(parse_client_range(Some("bytes=0-1,3-4")).is_err());
        assert!(parse_client_range(Some("items=0-1")).is_err());
        assert!(parse_client_range(Some("bytes=9-5")).is_err());

        let range = parse_content_range("bytes 5-9/20").unwrap();
        assert_eq!(
            (range.start, range.end, range.total),
            (Some(5), Some(9), Some(20))
        );
        let unsatisfied = parse_content_range("bytes */20").unwrap();
        assert_eq!(
            (unsatisfied.start, unsatisfied.end, unsatisfied.total),
            (None, None, Some(20))
        );
        assert!(parse_content_range("bytes 10-9/20").is_err());
    }

    #[test]
    fn loopback_proxy_requires_token_and_serves_range() {
        let body: Vec<u8> = (0..64 * 1024).map(|value| value as u8).collect();
        let source_body = body.clone();
        let source = MockServer::start(move |request| {
            respond_ranged(request, source_body.clone());
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend);
        let track = song("proxy-song");
        let url = manager.source(&track).expect("stream URL");

        let mut wrong = Url::parse(&url).expect("stream URL");
        wrong.query_pairs_mut().clear().extend_pairs([
            ("id".to_owned(), track.id.clone()),
            ("token".to_owned(), "wrong".to_owned()),
        ]);
        let client = Client::builder().build().unwrap();
        assert_eq!(client.get(wrong).send().unwrap().status().as_u16(), 404);

        let response = client
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=5-9")
            .send()
            .expect("proxy response");
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .unwrap(),
            "bytes 5-9/65536"
        );
        assert_eq!(response.bytes().unwrap().as_ref(), &body[5..10]);

        manager.release(&track.id);
        manager.close();
    }

    #[test]
    fn chunked_200_clean_eof_infers_total() {
        let body: Vec<u8> = (0..20_000).map(|value| value as u8).collect();
        let source_body = body.clone();
        let source = MockServer::start(move |request| {
            respond_chunked(request, source_body.clone());
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend);
        let track = song("chunked-song");
        let url = manager.source(&track).expect("stream URL");
        let response = Client::builder()
            .build()
            .unwrap()
            .get(url)
            .send()
            .expect("chunked proxy response");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some("20000")
        );
        assert_eq!(response.bytes().unwrap().as_ref(), body);

        manager.release(&track.id);
        manager.close();
    }

    #[test]
    fn background_download_is_saved_automatically() {
        let body: Vec<u8> = (0..128 * 1024).map(|value| value as u8).collect();
        let source_body = body.clone();
        let source = MockServer::start(move |request| {
            respond_ranged(request, source_body.clone());
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend.clone());
        let track = song("download-song");

        manager.download(&track).expect("queue download");
        assert!(wait_for(Duration::from_secs(4), || {
            manager
                .snapshot()
                .iter()
                .any(|status| status.song.id == track.id && status.state == COMPLETE)
        }));

        let status = manager
            .snapshot()
            .into_iter()
            .find(|status| status.song.id == track.id)
            .expect("completed transfer");
        assert_eq!(status.bytes, body.len() as u64);
        assert_eq!(status.total, body.len() as u64);
        let saves = lock(&backend.saves);
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0].extension, ".mp3");
        assert_eq!(fs::read(&saves[0].path).unwrap(), body);
        assert!(backend.local_path(&track.id).is_some());
        assert!(manager.source(&track).unwrap().ends_with(".mp3"));
        drop(saves);

        manager.release(&track.id);
        manager.close();
    }

    #[test]
    fn canceled_download_can_restart_before_old_worker_finishes() {
        let body = vec![7u8; 128 * 1024];
        let expected = body.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let source = MockServer::start(move |request| {
            if seen.fetch_add(1, Ordering::AcqRel) == 0 {
                thread::sleep(Duration::from_millis(200));
            }
            respond_ranged(request, body.clone());
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend.clone());
        let track = song("restart");
        manager.download(&track).unwrap();
        assert!(wait_for(Duration::from_secs(2), || requests
            .load(Ordering::Acquire)
            > 0));
        manager.cancel(&track.id);
        manager.download(&track).unwrap();
        assert!(wait_for(Duration::from_secs(4), || backend
            .local_path(&track.id)
            .is_some()));
        assert_eq!(
            fs::read(backend.local_path(&track.id).unwrap()).unwrap(),
            expected
        );
        assert_eq!(lock(&backend.saves).len(), 1);
        manager.close();
    }

    #[test]
    fn cancel_slow_download_stops_partial_cache() {
        let body: Vec<u8> = (0..512 * 1024).map(|value| value as u8).collect();
        let source_body = body.clone();
        let source = MockServer::start(move |request| {
            respond_slow_range(request, source_body.clone(), Duration::from_millis(3));
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend.clone());
        let track = song("cancel-song");

        manager.download(&track).expect("queue download");
        assert!(wait_for(Duration::from_secs(2), || {
            manager.snapshot().iter().any(|status| {
                status.song.id == track.id && matches!(status.state.as_str(), QUEUED | DOWNLOADING)
            })
        }));
        let cache_path = manager.cache_path(&track.id).expect("partial cache path");
        manager.cancel(&track.id);
        assert!(wait_for(Duration::from_secs(4), || {
            !manager
                .snapshot()
                .iter()
                .any(|status| status.song.id == track.id)
        }));
        assert!(!cache_path.exists());
        assert!(lock(&backend.saves).is_empty());

        manager.close();
    }

    #[test]
    fn close_is_bounded_when_proxy_client_stops_reading() {
        let body: Vec<u8> = (0..16 * 1024 * 1024).map(|value| value as u8).collect();
        let source_body = body.clone();
        let source = MockServer::start(move |request| {
            respond_ranged(request, source_body.clone());
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend);
        let track = song("unread-client");
        let url = Url::parse(&manager.source(&track).unwrap()).unwrap();
        let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        socket
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut header_bytes = [0u8; 4096];
        let _ = socket.read(&mut header_bytes);
        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        let temp_path = manager.temp_path();
        manager.close();
        assert!(
            started.elapsed() < Duration::from_millis(3500),
            "close took {:?}",
            started.elapsed()
        );
        assert!(!temp_path.exists());
        let _ = socket.shutdown(Shutdown::Both);
        drop(socket);
    }

    #[test]
    fn malformed_source_body_is_not_marked_complete() {
        let request_count = Arc::new(AtomicUsize::new(0));
        let count = request_count.clone();
        let source = MockServer::start(move |request| {
            count.fetch_add(1, Ordering::AcqRel);
            let response = HttpResponse::new(
                StatusCode(206),
                vec![
                    static_header("Content-Type", "audio/mpeg"),
                    static_header("Content-Range", "bytes 0-99/100"),
                ],
                Cursor::new(vec![1u8; 10]),
                Some(10),
                None,
            );
            let _ = request.respond(response);
        });
        let backend = Arc::new(MockBackend::new(&source));
        let manager = manager_for(backend.clone());
        let track = song("broken-source");
        let url = manager.source(&track).unwrap();
        let _ = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
            .get(url)
            .send();
        assert!(wait_for(Duration::from_secs(2), || {
            manager
                .snapshot()
                .iter()
                .any(|status| status.song.id == track.id && status.state == ERROR)
        }));
        assert!(lock(&backend.saves).is_empty());
        assert!(request_count.load(Ordering::Acquire) > 0);

        manager.release(&track.id);
        manager.close();
    }
}
