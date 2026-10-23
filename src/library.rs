use crate::{
    config::expand_home,
    core::{Album, Config, Song},
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use rand::random;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tempfile::NamedTempFile;
use url::Url;

const INDEX_VERSION: i32 = 1;
const METADATA_VERSION: i32 = 1;
const SUBSONIC_PAGE_SIZE: usize = 500;
const MAX_RESPONSE_SIZE: u64 = 64 << 20;
const MAX_MANIFEST_SIZE: u64 = 64 << 20;

#[derive(Default)]
struct LibraryState {
    albums: Vec<CachedAlbum>,
    songs: HashMap<String, Vec<CachedSong>>,
    metadata_ok: bool,
    own_records: HashMap<String, OwnRecord>,
    external: ExternalCatalog,
}

#[derive(Clone, Debug, Default)]
struct ExternalCatalog {
    songs: HashMap<String, PathBuf>,
    song_metadata: HashMap<String, Song>,
    albums: Vec<Album>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnIndex {
    #[serde(default)]
    version: i32,
    #[serde(
        default,
        rename = "songsBySongID",
        deserialize_with = "deserialize_or_default"
    )]
    songs_by_song_id: HashMap<String, OwnRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnRecord {
    #[serde(default)]
    song: Song,
    #[serde(default)]
    filename: String,
    #[serde(
        default,
        rename = "artworkFilename",
        skip_serializing_if = "String::is_empty"
    )]
    artwork_filename: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MetadataCache {
    #[serde(default)]
    version: i32,
    #[serde(default, deserialize_with = "deserialize_or_default")]
    albums: Vec<CachedAlbum>,
    #[serde(
        default,
        rename = "songsByAlbumID",
        deserialize_with = "deserialize_or_default"
    )]
    songs_by_album_id: HashMap<String, Vec<CachedSong>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CachedAlbum {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    genre: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    year: i32,
    #[serde(default, rename = "lastPlayed", skip_serializing_if = "is_zero_f64")]
    last_played: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CachedSong {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    artist: String,
    #[serde(default, rename = "albumID")]
    album_id: String,
    #[serde(
        default,
        rename = "albumTitle",
        skip_serializing_if = "String::is_empty"
    )]
    album_title: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    duration: f64,
    #[serde(default, rename = "trackNumber", skip_serializing_if = "is_zero_i32")]
    track_number: i32,
    #[serde(default, rename = "discNumber", skip_serializing_if = "is_zero_i32")]
    disc_number: i32,
}

#[derive(Debug, Deserialize)]
struct YoruneManifest {
    #[serde(default, rename = "version")]
    _version: i32,
    #[serde(default, rename = "filenamesBySongID")]
    filenames_by_song_id: Option<HashMap<String, String>>,
    #[serde(
        default,
        rename = "songsBySongID",
        deserialize_with = "deserialize_or_default"
    )]
    songs_by_song_id: HashMap<String, YoruneSong>,
}

#[derive(Debug, Default, Deserialize)]
struct YoruneSong {
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default, rename = "albumID")]
    album_id: String,
    #[serde(default, rename = "albumTitle")]
    album_title: String,
    #[serde(default)]
    duration: f64,
    #[serde(default, rename = "trackNumber")]
    track_number: Option<i32>,
    #[serde(default, rename = "discNumber")]
    disc_number: Option<i32>,
}

/// Akari's remote catalog, downloadable index, and read-only external library.
///
/// Network calls use the blocking client and are intentionally suitable for
/// background threads. All public methods take `&self`.
pub struct Library {
    audio_dir: PathBuf,
    cache_dir: PathBuf,
    index_path: PathBuf,
    metadata_path: PathBuf,
    external_dirs: Vec<PathBuf>,
    server_url: String,
    username: String,
    password: String,
    auth_salt: String,
    auth_token: String,
    client: Client,
    state: Mutex<LibraryState>,
    metadata_write_lock: Mutex<()>,
    write_lock: Mutex<()>,
}

impl Library {
    /// Constructs a library and loads its metadata, download index, and external
    /// catalogs. Remote access is deferred until `albums` or `songs` needs it.
    pub fn new(cfg: Config) -> Result<Self> {
        Self::new_with_cache_root(cfg, None)
    }

    fn new_with_cache_root(cfg: Config, cache_root_override: Option<&Path>) -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("determine home directory")?;

        let raw_music_dir = cfg.storage.music_dir.trim();
        let music_dir = if raw_music_dir.is_empty() {
            home.join(".local/share/akari/music")
        } else {
            expand_home(raw_music_dir, &home)
        };
        let music_dir = absolute_path(music_dir)?;

        let external_dirs = cfg
            .storage
            .external_dirs
            .iter()
            .map(|path| expand_home(path, &home))
            .collect();

        // Keep the exact Go behavior: no trimming, URL normalization, or
        // username normalization participates in the namespace.
        let namespace = namespace_for(&cfg.server.url, &cfg.server.username);
        let cache_root = cache_root_override.map(Path::to_owned).unwrap_or_else(|| {
            env::var("XDG_CACHE_HOME")
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| expand_home(&value, &home))
                .unwrap_or_else(|| home.join(".cache"))
        });
        let cache_dir = absolute_path(cache_root.join("akari").join(&namespace))?;

        let namespace_dir = music_dir.join(&namespace);
        let audio_dir = namespace_dir.join("audio");
        secure_create_dir_all(&audio_dir).context("create music directory")?;
        secure_create_dir_all(&cache_dir).context("create cache directory")?;

        let salt_bytes: [u8; 16] = random();
        let auth_salt = hex_encode(&salt_bytes);
        let auth_token =
            hex_encode(&md5::compute(format!("{}{}", cfg.server.password, auth_salt)).0);
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("initialize HTTP client")?;

        let library = Self {
            audio_dir,
            cache_dir: cache_dir.clone(),
            index_path: namespace_dir.join("index.json"),
            metadata_path: cache_dir.join("metadata.json"),
            external_dirs,
            server_url: cfg.server.url,
            username: cfg.server.username,
            password: cfg.server.password,
            auth_salt,
            auth_token,
            client,
            state: Mutex::new(LibraryState::default()),
            metadata_write_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        };

        library.load_index()?;
        library.load_metadata();
        library.reload_external()?;
        Ok(library)
    }

    /// Returns the remote catalog, cached metadata, and local catalogs. When a
    /// refresh fails, any usable cached or local catalog is returned.
    pub fn albums(&self, refresh: bool) -> Result<Vec<Album>> {
        self.reload_external()?;
        let (have_metadata, cached_albums) = {
            let state = lock(&self.state);
            (state.metadata_ok, state.albums.clone())
        };

        let mut remote_error = None;
        let mut candidate_albums = cached_albums;
        if (refresh || !have_metadata) && self.has_server() {
            match self.fetch_albums() {
                Ok(albums) => {
                    candidate_albums = albums.clone();
                    if let Err(error) = self.store_metadata_albums(albums) {
                        remote_error = Some(error);
                    }
                }
                Err(error) => remote_error = Some(error),
            }
        }

        let combined = self.combined_albums(&candidate_albums);
        if !combined.is_empty() {
            return Ok(combined);
        }
        if let Some(error) = remote_error {
            return Err(error).context("load albums");
        }
        bail!(
            "no music library is available; configure [server] or add a cached or external library"
        )
    }

    /// Returns one album. Cached songs are preferred, while external and owned
    /// local files fill in songs that are not part of the remote cache.
    pub fn songs(&self, album_id: &str) -> Result<Vec<Song>> {
        if album_id.trim().is_empty() {
            bail!("album ID is empty");
        }
        self.reload_external()?;

        let cached = {
            let state = lock(&self.state);
            state.songs.get(album_id).cloned()
        };
        let mut remote_error = None;
        let mut candidate_songs = cached.clone().unwrap_or_default();
        if cached.is_none() && self.has_server() {
            match self.fetch_songs(album_id) {
                Ok((songs, album)) => {
                    candidate_songs = songs.clone();
                    if let Err(error) = self.store_metadata_songs(album_id, songs, album) {
                        remote_error = Some(error);
                    }
                }
                Err(error) => remote_error = Some(error),
            }
        }

        let local_cached = {
            let state = lock(&self.state);
            state.songs.get(album_id).cloned().unwrap_or_default()
        };
        let combined = self.combined_songs(album_id, &candidate_songs, &local_cached);
        if !combined.is_empty() {
            return Ok(combined);
        }
        if let Some(error) = remote_error {
            return Err(error).with_context(|| format!("load songs for album {album_id:?}"));
        }
        bail!("no songs are available for album {album_id:?}")
    }

    /// Returns albums with at least one readable owned or external file.
    pub fn downloaded_albums(&self) -> Vec<Album> {
        let state = lock(&self.state);
        let mut by_id = HashMap::<String, Album>::new();

        for album in &state.external.albums {
            if external_album_has_file(&state.external, &album.id) {
                by_id.insert(album.id.clone(), album.clone());
            }
        }
        for record in state.own_records.values() {
            let song = &record.song;
            if song.album_id.is_empty() || !regular_file(&self.audio_dir.join(&record.filename)) {
                continue;
            }
            by_id.entry(song.album_id.clone()).or_insert_with(|| Album {
                id: song.album_id.clone(),
                title: song.album_title.clone(),
                artist: song.artist.clone(),
                ..Album::default()
            });
        }

        let mut albums: Vec<_> = by_id.into_values().collect();
        sort_albums(&mut albums);
        albums
    }

    /// Returns readable local songs. An empty album ID returns every local song.
    pub fn downloaded_songs(&self, album_id: &str) -> Vec<Song> {
        let state = lock(&self.state);
        let mut by_id = HashMap::<String, Song>::new();

        for (song_id, path) in &state.external.songs {
            if !regular_file(path) {
                continue;
            }
            if let Some(song) = state.external.song_metadata.get(song_id)
                && (album_id.is_empty() || song.album_id == album_id)
            {
                by_id.insert(song_id.clone(), song.clone());
            }
        }
        for (song_id, record) in &state.own_records {
            if !album_id.is_empty() && record.song.album_id != album_id {
                continue;
            }
            if regular_file(&self.audio_dir.join(&record.filename)) {
                by_id.insert(song_id.clone(), record.song.clone());
            }
        }

        let mut songs: Vec<_> = by_id.into_values().collect();
        sort_songs(&mut songs);
        songs
    }

    /// Returns a readable owned file first, then a readable external file.
    pub fn local_path(&self, song_id: &str) -> Option<PathBuf> {
        let state = lock(&self.state);
        if let Some(record) = state.own_records.get(song_id) {
            let path = self.audio_dir.join(&record.filename);
            if regular_file(&path) {
                return Some(path);
            }
        }
        state
            .external
            .songs
            .get(song_id)
            .filter(|path| regular_file(path))
            .cloned()
    }

    /// Builds a token-authenticated Subsonic stream URL.
    pub fn stream_url(&self, song_id: &str) -> Result<String> {
        if song_id.trim().is_empty() {
            bail!("song ID is empty");
        }
        self.endpoint("stream", &[("id", song_id), ("format", "raw")])
    }

    /// Builds a token-authenticated Subsonic download URL.
    pub fn download_url(&self, song_id: &str) -> Result<String> {
        if song_id.trim().is_empty() {
            bail!("song ID is empty");
        }
        self.endpoint("download", &[("id", song_id), ("format", "raw")])
    }

    /// Returns true only for an Akari-managed, readable downloaded file.
    pub fn is_owned(&self, song_id: &str) -> bool {
        let state = lock(&self.state);
        state
            .own_records
            .get(song_id)
            .is_some_and(|record| regular_file(&self.audio_dir.join(&record.filename)))
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn http_client(&self) -> Client {
        self.client.clone()
    }

    /// Atomically installs a completed download and updates the JSON index.
    pub fn save_download(&self, song: &Song, temp: &Path, extension: &str) -> Result<()> {
        if song.id.trim().is_empty() {
            bail!("song ID is empty");
        }
        let extension = normalize_extension(extension, temp)?;
        let metadata = fs::metadata(temp).context("open downloaded audio")?;
        if !metadata.is_file() {
            bail!("downloaded audio is not a regular file");
        }

        let mut staged =
            NamedTempFile::new_in(&self.audio_dir).context("create temporary download")?;
        set_file_mode(staged.as_file(), 0o600)?;
        {
            let mut source = File::open(temp).context("open downloaded audio")?;
            std::io::copy(&mut source, &mut staged).context("copy downloaded audio")?;
        }
        staged
            .as_file_mut()
            .flush()
            .context("flush downloaded audio")?;
        staged
            .as_file()
            .sync_all()
            .context("sync downloaded audio")?;

        let filename = format!(
            "{}.{}",
            hex_encode(&Sha256::digest(song.id.as_bytes())),
            extension
        );
        let final_path = self.audio_dir.join(&filename);

        let _write_guard = lock(&self.write_lock);
        let mut state = lock(&self.state);
        let old_record = state.own_records.get(&song.id).cloned();
        let mut backups = HashMap::<PathBuf, PathBuf>::new();

        let mut paths_to_backup = vec![final_path.clone()];
        if let Some(record) = &old_record {
            let old_path = self.audio_dir.join(&record.filename);
            if old_path != final_path {
                paths_to_backup.push(old_path);
            }
        }
        let unique_paths: HashSet<_> = paths_to_backup.into_iter().collect();

        for path in unique_paths {
            if regular_file(&path) {
                match backup_file(&path) {
                    Ok(backup) => {
                        backups.insert(path, backup);
                    }
                    Err(error) => {
                        rollback_file_changes(&final_path, false, &backups);
                        return Err(error).context("prepare download replacement");
                    }
                }
            }
        }

        if let Err(error) = staged.persist(&final_path) {
            rollback_file_changes(&final_path, false, &backups);
            return Err(error.error).context("install downloaded audio");
        }
        sync_directory(&self.audio_dir);

        state.own_records.insert(
            song.id.clone(),
            OwnRecord {
                song: song.clone(),
                filename,
                artwork_filename: String::new(),
            },
        );
        if let Err(error) = self.persist_index_locked(&state) {
            rollback_file_changes(&final_path, true, &backups);
            match old_record {
                Some(record) => {
                    state.own_records.insert(song.id.clone(), record);
                }
                None => {
                    state.own_records.remove(&song.id);
                }
            }
            return Err(error);
        }

        for backup in backups.into_values() {
            let _ = fs::remove_file(backup);
        }
        sync_directory(&self.audio_dir);
        if let Some(parent) = self.index_path.parent() {
            sync_directory(parent);
        }
        Ok(())
    }

    /// Removes only an Akari-managed file and its index entry. External files
    /// are read-only and are never deleted.
    pub fn remove(&self, song_id: &str) -> Result<()> {
        let _write_guard = lock(&self.write_lock);
        let mut state = lock(&self.state);
        let Some(record) = state.own_records.get(song_id).cloned() else {
            return Ok(());
        };

        let path = self.audio_dir.join(&record.filename);
        let backup = if regular_file(&path) {
            Some(backup_file(&path).context("prepare downloaded file removal")?)
        } else {
            None
        };

        state.own_records.remove(song_id);
        if let Err(error) = self.persist_index_locked(&state) {
            state.own_records.insert(song_id.to_owned(), record);
            if let Some(backup) = &backup {
                let _ = fs::rename(backup, &path);
            }
            return Err(error);
        }

        if let Some(backup) = backup {
            fs::remove_file(&backup).context("remove downloaded audio")?;
        }
        if safe_filename(&record.artwork_filename)
            && !album_artwork_in_use(&state.own_records, &record.song.album_id)
        {
            let artwork_path = self.audio_dir.join(&record.artwork_filename);
            if regular_file(&artwork_path) {
                fs::remove_file(&artwork_path).context("remove album artwork")?;
            }
        }
        sync_directory(&self.audio_dir);
        if let Some(parent) = self.index_path.parent() {
            sync_directory(parent);
        }
        Ok(())
    }

    fn has_server(&self) -> bool {
        !self.server_url.trim().is_empty()
    }

    fn load_index(&self) -> Result<()> {
        let data = match fs::read(&self.index_path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("read download index"),
        };
        let mut index: OwnIndex = serde_json::from_slice(&data).with_context(|| {
            format!(
                "parse download index {:?}: invalid JSON",
                self.index_path.display()
            )
        })?;
        if index.version != 0 && index.version != INDEX_VERSION {
            bail!(
                "parse download index {:?}: unsupported version {}",
                self.index_path.display(),
                index.version
            );
        }

        index.songs_by_song_id.retain(|song_id, record| {
            if !safe_filename(&record.filename) {
                return false;
            }
            if record.song.id.is_empty() {
                record.song.id = song_id.clone();
            }
            true
        });
        lock(&self.state).own_records = index.songs_by_song_id;
        Ok(())
    }

    fn load_metadata(&self) {
        let Ok(data) = fs::read(&self.metadata_path) else {
            return;
        };
        let Ok(mut cache) = serde_json::from_slice::<MetadataCache>(&data) else {
            return;
        };
        if cache.version != 0 && cache.version != METADATA_VERSION {
            return;
        }
        let mut state = lock(&self.state);
        state.albums = std::mem::take(&mut cache.albums);
        state.songs = std::mem::take(&mut cache.songs_by_album_id);
        state.metadata_ok = true;
    }

    fn store_metadata_albums(&self, albums: Vec<CachedAlbum>) -> Result<()> {
        let _guard = lock(&self.metadata_write_lock);
        let snapshot = {
            let mut state = lock(&self.state);
            state.albums = albums;
            state.metadata_ok = true;
            metadata_snapshot(&state)
        };
        let data = serde_json::to_vec(&snapshot).context("encode album cache")?;
        write_file_atomic(&self.metadata_path, &data, 0o600).context("write album cache")
    }

    fn store_metadata_songs(
        &self,
        album_id: &str,
        songs: Vec<CachedSong>,
        album: CachedAlbum,
    ) -> Result<()> {
        let _guard = lock(&self.metadata_write_lock);
        let snapshot = {
            let mut state = lock(&self.state);
            state.songs.insert(album_id.to_owned(), songs);
            state.metadata_ok = true;
            if !album.id.is_empty() && !state.albums.iter().any(|item| item.id == album.id) {
                state.albums.push(album);
            }
            metadata_snapshot(&state)
        };
        let data = serde_json::to_vec(&snapshot).context("encode song cache")?;
        write_file_atomic(&self.metadata_path, &data, 0o600).context("write song cache")
    }

    fn combined_albums(&self, candidates: &[CachedAlbum]) -> Vec<Album> {
        let state = lock(&self.state);
        let mut by_id = HashMap::<String, Album>::new();
        for album in candidates {
            let album = album_from_cached(album);
            by_id.insert(album.id.clone(), album);
        }
        for album in &state.external.albums {
            by_id
                .entry(album.id.clone())
                .or_insert_with(|| album.clone());
        }
        for record in state.own_records.values() {
            let song = &record.song;
            if song.album_id.is_empty() {
                continue;
            }
            by_id.entry(song.album_id.clone()).or_insert_with(|| Album {
                id: song.album_id.clone(),
                title: song.album_title.clone(),
                artist: song.artist.clone(),
                ..Album::default()
            });
        }
        let mut albums: Vec<_> = by_id.into_values().collect();
        sort_albums(&mut albums);
        albums
    }

    fn combined_songs(
        &self,
        album_id: &str,
        candidates: &[CachedSong],
        cached: &[CachedSong],
    ) -> Vec<Song> {
        let state = lock(&self.state);
        let mut by_id = HashMap::<String, Song>::new();
        for song in candidates.iter().chain(cached) {
            let song = song_from_cached(song);
            by_id.insert(song.id.clone(), song);
        }
        for (song_id, path) in &state.external.songs {
            if !regular_file(path) {
                continue;
            }
            if let Some(song) = state.external.song_metadata.get(song_id)
                && song.album_id == album_id
            {
                by_id.insert(song_id.clone(), song.clone());
            }
        }
        for (song_id, record) in &state.own_records {
            if record.song.album_id == album_id
                && regular_file(&self.audio_dir.join(&record.filename))
            {
                by_id.insert(song_id.clone(), record.song.clone());
            }
        }
        let mut songs: Vec<_> = by_id.into_values().collect();
        sort_songs(&mut songs);
        songs
    }

    fn persist_index_locked(&self, state: &LibraryState) -> Result<()> {
        let index = OwnIndex {
            version: INDEX_VERSION,
            songs_by_song_id: state.own_records.clone(),
        };
        let data = serde_json::to_vec(&index).context("encode download index")?;
        write_file_atomic(&self.index_path, &data, 0o600).context("write download index")
    }

    fn reload_external(&self) -> Result<()> {
        let catalog = scan_external(&self.external_dirs);
        lock(&self.state).external = catalog;
        Ok(())
    }

    fn fetch_albums(&self) -> Result<Vec<CachedAlbum>> {
        let mut albums = Vec::new();
        for offset in (0usize..).step_by(SUBSONIC_PAGE_SIZE) {
            let page_size = SUBSONIC_PAGE_SIZE.to_string();
            let offset = offset.to_string();
            let values = [
                ("type", "alphabeticalByName"),
                ("size", page_size.as_str()),
                ("offset", offset.as_str()),
                ("f", "json"),
            ];
            let envelope = self.get_subsonic::<SubsonicEnvelope>("getAlbumList2", &values)?;
            check_subsonic_response(&envelope.response, self)?;
            let list = envelope
                .response
                .album_list2
                .context("server returned an invalid album list")?;
            let page = list.albums.into_vec();
            albums.extend(
                page.iter()
                    .filter(|album| !album.id.is_empty())
                    .map(|album| CachedAlbum {
                        id: album.id.clone(),
                        title: album.name.clone(),
                        artist: album.artist.clone(),
                        genre: album.genre.trim().to_owned(),
                        year: album.year.unwrap_or_default(),
                        last_played: album
                            .played
                            .as_ref()
                            .and_then(played_date)
                            .and_then(parse_played)
                            .unwrap_or_default(),
                    }),
            );
            if page.len() < SUBSONIC_PAGE_SIZE {
                break;
            }
        }
        Ok(albums)
    }

    fn fetch_songs(&self, album_id: &str) -> Result<(Vec<CachedSong>, CachedAlbum)> {
        let envelope =
            self.get_subsonic::<SubsonicEnvelope>("getAlbum", &[("id", album_id), ("f", "json")])?;
        check_subsonic_response(&envelope.response, self)?;
        let detail = envelope
            .response
            .album
            .context("server returned an invalid album")?;
        let items = detail.songs.into_vec();
        let mut songs = Vec::with_capacity(items.len());
        let mut artists = Vec::with_capacity(items.len());
        for item in items {
            if item.id.is_empty() {
                continue;
            }
            let album_title = if item.album.is_empty() {
                detail.name.clone()
            } else {
                item.album
            };
            songs.push(CachedSong {
                id: item.id,
                title: item.title,
                artist: item.artist.clone(),
                album_id: album_id.to_owned(),
                album_title,
                duration: item.duration.unwrap_or_default(),
                track_number: item.track.unwrap_or_default(),
                disc_number: item.disc_number.unwrap_or_default(),
            });
            artists.push(item.artist);
        }

        let mut album = CachedAlbum {
            id: album_id.to_owned(),
            title: detail.name,
            artist: artists
                .iter()
                .map(|value| value.trim())
                .find(|value| !value.is_empty())
                .unwrap_or_default()
                .to_owned(),
            ..CachedAlbum::default()
        };
        if album.title.is_empty()
            && let Some(song) = songs.first()
        {
            album.title = song.album_title.clone();
        }
        if album.artist.is_empty()
            && let Some(song) = songs.first()
        {
            album.artist = song.artist.clone();
        }
        Ok((songs, album))
    }

    fn get_subsonic<T: DeserializeOwned>(
        &self,
        action: &str,
        values: &[(&str, &str)],
    ) -> Result<T> {
        let request_url = self.endpoint(action, values)?;
        let mut response = self
            .client
            .get(request_url)
            .send()
            .map_err(|error| safe_transport_error(&error))?;
        if !response.status().is_success() {
            bail!("server returned HTTP {}", response.status().as_u16());
        }
        let mut data = Vec::new();
        response
            .by_ref()
            .take(MAX_RESPONSE_SIZE + 1)
            .read_to_end(&mut data)
            .context("read server response failed")?;
        if data.len() as u64 > MAX_RESPONSE_SIZE {
            bail!("server response is too large");
        }
        serde_json::from_slice(&data).context("server returned an invalid JSON response")
    }

    fn endpoint(&self, action: &str, values: &[(&str, &str)]) -> Result<String> {
        if !self.has_server() {
            bail!("no server is configured");
        }
        let mut endpoint = Url::parse(&self.server_url)
            .context("server URL must be an absolute HTTP or HTTPS URL")?;
        if endpoint.host_str().is_none() || !matches!(endpoint.scheme(), "http" | "https") {
            bail!("server URL must be an absolute HTTP or HTTPS URL");
        }
        endpoint.set_fragment(None);
        endpoint.set_path(&format!(
            "{}/rest/{action}.view",
            endpoint.path().trim_end_matches('/')
        ));

        let mut query: Vec<(String, String)> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        for (key, value) in [
            ("u", self.username.as_str()),
            ("t", self.auth_token.as_str()),
            ("s", self.auth_salt.as_str()),
            ("v", "1.16.1"),
            ("c", "Akari"),
        ]
        .into_iter()
        .chain(values.iter().copied())
        {
            set_query_value(&mut query, key, value);
        }
        endpoint.set_query(None);
        {
            let mut pairs = endpoint.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(&key, &value);
            }
        }
        Ok(endpoint.to_string())
    }
}

fn namespace_for(server_url: &str, username: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(server_url.as_bytes());
    hasher.update(b"\n");
    hasher.update(username.as_bytes());
    hex_encode(&hasher.finalize())
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()
            .context("determine current directory")?
            .join(path))
    }
}

fn secure_create_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn set_file_mode(file: &File, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (file, mode);
    Ok(())
}

fn write_file_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("determine file parent")?;
    secure_create_dir_all(parent)?;
    let mut file = NamedTempFile::new_in(parent).context("create temporary JSON file")?;
    set_file_mode(file.as_file(), mode)?;
    file.write_all(data)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    sync_directory(parent);
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn backup_file(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("determine backup parent")?;
    let temporary = NamedTempFile::new_in(parent)?;
    let (temporary_file, backup) = temporary
        .keep()
        .map_err(|error| error.error)
        .context("create backup file")?;
    drop(temporary_file);
    if let Err(error) = fs::rename(path, &backup) {
        let _ = fs::remove_file(&backup);
        return Err(error).context("move original file to backup");
    }
    Ok(backup)
}

fn rollback_file_changes(final_path: &Path, installed: bool, backups: &HashMap<PathBuf, PathBuf>) {
    if installed {
        let _ = fs::remove_file(final_path);
    }
    for backup in backups.values() {
        let _ = fs::remove_file(backup);
    }
    for (original, backup) in backups {
        let _ = fs::rename(backup, original);
    }
}

fn normalize_extension(extension: &str, temp_path: &Path) -> Result<String> {
    let mut extension = extension.trim();
    if extension.is_empty() {
        extension = temp_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
    }
    let extension = extension.trim_start_matches('.');
    if extension.is_empty() {
        return Ok("audio".to_owned());
    }
    if extension.len() > 10 {
        bail!("download extension is too long");
    }
    if !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        bail!("download extension {extension:?} is invalid");
    }
    Ok(extension.to_ascii_lowercase())
}

fn metadata_snapshot(state: &LibraryState) -> MetadataCache {
    MetadataCache {
        version: METADATA_VERSION,
        albums: state.albums.clone(),
        songs_by_album_id: state.songs.clone(),
    }
}

fn album_from_cached(album: &CachedAlbum) -> Album {
    Album {
        id: album.id.clone(),
        title: album.title.clone(),
        artist: album.artist.clone(),
        genre: album.genre.clone(),
        year: album.year,
        last_played: album.last_played,
    }
}

fn song_from_cached(song: &CachedSong) -> Song {
    Song {
        id: song.id.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        album_id: song.album_id.clone(),
        album_title: song.album_title.clone(),
        duration: song.duration,
        track_number: song.track_number,
        disc_number: song.disc_number,
    }
}

fn sort_albums(albums: &mut [Album]) {
    albums.sort_by(|left, right| {
        left.title
            .trim()
            .to_lowercase()
            .cmp(&right.title.trim().to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn sort_songs(songs: &mut [Song]) {
    songs.sort_by(|left, right| {
        let left_disc = if left.disc_number == 0 {
            1
        } else {
            left.disc_number
        };
        let right_disc = if right.disc_number == 0 {
            1
        } else {
            right.disc_number
        };
        left_disc.cmp(&right_disc).then_with(|| {
            match (left.track_number, right.track_number) {
                (0, 0) => std::cmp::Ordering::Equal,
                (0, _) => std::cmp::Ordering::Greater,
                (_, 0) => std::cmp::Ordering::Less,
                (left_track, right_track) => left_track.cmp(&right_track),
            }
            .then_with(|| {
                left.title
                    .trim()
                    .to_lowercase()
                    .cmp(&right.title.trim().to_lowercase())
            })
            .then_with(|| left.id.cmp(&right.id))
        })
    });
}

fn album_artwork_in_use(records: &HashMap<String, OwnRecord>, album_id: &str) -> bool {
    records
        .values()
        .any(|record| record.song.album_id == album_id && !record.artwork_filename.is_empty())
}

fn external_album_has_file(catalog: &ExternalCatalog, album_id: &str) -> bool {
    catalog.songs.iter().any(|(song_id, path)| {
        catalog
            .song_metadata
            .get(song_id)
            .is_some_and(|song| song.album_id == album_id)
            && regular_file(path)
    })
}

fn scan_external(roots: &[PathBuf]) -> ExternalCatalog {
    let mut catalog = ExternalCatalog::default();
    for root in roots {
        let root = root.to_string_lossy();
        let root = root.trim();
        if root.is_empty() {
            continue;
        }
        for directory in external_directories(Path::new(root)) {
            load_external_directory(&directory, &mut catalog);
        }
    }
    catalog.albums = albums_from_external(&catalog.song_metadata);
    catalog
}

fn external_directories(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    if has_external_index(root) {
        return vec![root.to_owned()];
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut directories: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && has_external_index(path))
        .collect();
    directories.sort();
    directories
}

fn has_external_index(directory: &Path) -> bool {
    regular_file(&directory.join("manifest.json")) || regular_file(&directory.join("index.json"))
}

fn load_external_directory(directory: &Path, catalog: &mut ExternalCatalog) {
    let manifest_path = directory.join("manifest.json");
    if regular_file(&manifest_path) {
        if let Ok(data) = read_limited_file(&manifest_path) {
            load_yorune_manifest(directory, &data, catalog);
        }
        return;
    }
    let index_path = directory.join("index.json");
    if let Ok(data) = read_limited_file(&index_path) {
        load_external_own_index(directory, &data, catalog);
    }
}

fn load_yorune_manifest(directory: &Path, data: &[u8], catalog: &mut ExternalCatalog) {
    let (filenames, songs_by_id) = match serde_json::from_slice::<YoruneManifest>(data) {
        Ok(mut manifest) if manifest.filenames_by_song_id.is_some() => (
            manifest.filenames_by_song_id.take().unwrap_or_default(),
            manifest.songs_by_song_id,
        ),
        _ => match serde_json::from_slice::<HashMap<String, String>>(data) {
            Ok(legacy) => (legacy, HashMap::new()),
            Err(_) => return,
        },
    };

    for (song_id, filename) in filenames {
        if song_id.is_empty() || !safe_filename(&filename) {
            continue;
        }
        let Some(path) = resolve_external_file(directory, &filename) else {
            continue;
        };
        catalog.songs.entry(song_id.clone()).or_insert(path);
        let Some(stored) = songs_by_id.get(&song_id) else {
            continue;
        };
        let mut song = Song {
            id: song_id.clone(),
            title: stored.title.clone(),
            artist: stored.artist.clone(),
            album_id: stored.album_id.clone(),
            album_title: stored.album_title.clone(),
            duration: stored.duration,
            track_number: stored.track_number.unwrap_or_default(),
            disc_number: stored.disc_number.unwrap_or_default(),
        };
        // Go identifies Yorune entries by the map key and ignores the
        // optional song object's own `id`.
        song.id = song_id.clone();
        if song.title.is_empty() {
            song.title = song_id.clone();
        }
        catalog.song_metadata.entry(song_id).or_insert(song);
    }
}

fn load_external_own_index(directory: &Path, data: &[u8], catalog: &mut ExternalCatalog) {
    let Ok(mut index) = serde_json::from_slice::<OwnIndex>(data) else {
        return;
    };
    for (song_id, record) in &mut index.songs_by_song_id {
        if song_id.is_empty() || !safe_filename(&record.filename) {
            continue;
        }
        let Some(path) = resolve_external_file(directory, &record.filename) else {
            continue;
        };
        catalog.songs.entry(song_id.clone()).or_insert(path);
        if record.song.id.is_empty() {
            record.song.id = song_id.clone();
        }
        if record.song.title.is_empty() {
            record.song.title = song_id.clone();
        }
        catalog
            .song_metadata
            .entry(song_id.clone())
            .or_insert_with(|| record.song.clone());
    }
}

fn albums_from_external(songs: &HashMap<String, Song>) -> Vec<Album> {
    let mut by_id = HashMap::<String, Album>::new();
    for song in songs.values() {
        if song.album_id.is_empty() {
            continue;
        }
        by_id.entry(song.album_id.clone()).or_insert_with(|| Album {
            id: song.album_id.clone(),
            title: if song.album_title.is_empty() {
                song.album_id.clone()
            } else {
                song.album_title.clone()
            },
            artist: song.artist.clone(),
            ..Album::default()
        });
    }
    let mut albums: Vec<_> = by_id.into_values().collect();
    sort_albums(&mut albums);
    albums
}

fn read_limited_file(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut data = Vec::new();
    file.take(MAX_MANIFEST_SIZE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_MANIFEST_SIZE {
        bail!("manifest is too large");
    }
    Ok(data)
}

fn resolve_external_file(directory: &Path, filename: &str) -> Option<PathBuf> {
    if !safe_filename(filename) {
        return None;
    }
    [
        directory.join(filename),
        directory.join("audio").join(filename),
        directory.join("artwork").join(filename),
    ]
    .into_iter()
    .find(|path| regular_file(path))
}

fn safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', '\0'])
        && Path::new(value)
            .file_name()
            .is_some_and(|name| name == value)
}

fn regular_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_played(value: String) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    parsed
        .timestamp_nanos_opt()
        .map(|nanos| nanos as f64 / 1_000_000_000.0)
}

fn safe_transport_error(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("server request timed out")
    } else if error.is_connect() {
        anyhow!("server could not be reached")
    } else {
        anyhow!("server request failed")
    }
}

fn check_subsonic_response(response: &SubsonicResponse, library: &Library) -> Result<()> {
    if response.status == "ok" {
        return Ok(());
    }
    if let Some(error) = &response.error {
        if error.code == 40 || error.code == 50 {
            bail!("server rejected the username or password");
        }
        let message = error.message.trim();
        if !message.is_empty() {
            bail!("server error {}: {}", error.code, redact(library, message));
        }
        bail!("server error {}", error.code);
    }
    bail!("server returned a failed response");
}

fn redact(library: &Library, value: &str) -> String {
    let mut value = value.trim().to_owned();
    for secret in [
        library.password.as_str(),
        library.username.as_str(),
        library.auth_salt.as_str(),
        library.auth_token.as_str(),
        library.server_url.as_str(),
    ] {
        if !secret.is_empty() {
            value = value.replace(secret, "[redacted]");
        }
    }
    if value.chars().count() > 240 {
        value = value.chars().take(240).collect::<String>() + "...";
    }
    if value.is_empty() {
        "server request failed".to_owned()
    } else {
        value
    }
}

fn set_query_value(query: &mut Vec<(String, String)>, key: &str, value: &str) {
    query.retain(|(existing, _)| existing != key);
    query.push((key.to_owned(), value.to_owned()));
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

fn deserialize_or_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicEnvelope {
    #[serde(default, rename = "subsonic-response")]
    response: SubsonicResponse,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicResponse {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "albumList2")]
    album_list2: Option<SubsonicAlbumList>,
    #[serde(default)]
    album: Option<SubsonicAlbumDetail>,
    #[serde(default)]
    error: Option<SubsonicError>,
}

#[derive(Debug, Deserialize)]
struct SubsonicError {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicAlbumList {
    #[serde(default, rename = "album")]
    albums: OneOrMany<SubsonicAlbum>,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicAlbum {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    genre: String,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    played: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicAlbumDetail {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "song")]
    songs: OneOrMany<SubsonicSong>,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicSong {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    track: Option<i32>,
    #[serde(default, rename = "discNumber")]
    disc_number: Option<i32>,
}

#[derive(Debug, Default)]
enum OneOrMany<T> {
    #[default]
    None,
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::None => Vec::new(),
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for OneOrMany<T> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Null => Ok(Self::None),
            Value::Array(_) => serde_json::from_value(value)
                .map(Self::Many)
                .map_err(serde::de::Error::custom),
            _ => serde_json::from_value(value)
                .map(Self::One)
                .map_err(serde::de::Error::custom),
        }
    }
}

fn played_date(value: &Value) -> Option<String> {
    match value {
        Value::String(date) => Some(date.clone()),
        Value::Object(object) => object
            .get("date")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::{Response, Server};

    fn test_config(root: &Path) -> Config {
        Config {
            storage: crate::core::StorageConfig {
                music_dir: root.join("music").to_string_lossy().into_owned(),
                external_dirs: Vec::new(),
            },
            ..Config::default()
        }
    }

    fn test_library(cfg: Config, root: &Path) -> Result<Library> {
        Library::new_with_cache_root(cfg, Some(&root.join("xdg-cache")))
    }

    fn song(id: &str, album: &str, track: i32) -> Song {
        Song {
            id: id.to_owned(),
            title: format!("Song {track}"),
            artist: "Artist".to_owned(),
            album_id: album.to_owned(),
            album_title: "Album".to_owned(),
            duration: 12.5,
            track_number: track,
            disc_number: 1,
        }
    }

    #[test]
    fn namespace_matches_go_hash_without_normalizing_inputs() {
        assert_eq!(
            namespace_for("https://music.example/subsonic?", "user"),
            "5cbc1b61783807350ff72308fb37884cd8298751e64d8e75f66df4fe4f022ccc"
        );
        assert_ne!(
            namespace_for(" https://music.example ", "user"),
            namespace_for("https://music.example", "user")
        );
    }

    #[test]
    fn subsonic_accepts_arrays_single_objects_and_nulls() {
        let array: SubsonicEnvelope = serde_json::from_str(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[{"id":"a"},{"id":"b"}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            array.response.album_list2.unwrap().albums.into_vec().len(),
            2
        );

        let one: SubsonicEnvelope = serde_json::from_str(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":{"id":"a"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            one.response.album_list2.unwrap().albums.into_vec()[0].id,
            "a"
        );

        let single_song: SubsonicEnvelope = serde_json::from_str(
            r#"{"subsonic-response":{"status":"ok","album":{"name":"A","song":{"id":"s","album":"A","track":1}}}}"#,
        )
        .unwrap();
        assert_eq!(
            single_song.response.album.unwrap().songs.into_vec()[0].id,
            "s"
        );

        let empty: SubsonicEnvelope = serde_json::from_str(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":null}}}"#,
        )
        .unwrap();
        assert!(
            empty
                .response
                .album_list2
                .unwrap()
                .albums
                .into_vec()
                .is_empty()
        );
    }

    #[test]
    fn json_formats_remain_go_compatible() {
        let song = song("s", "a", 2);
        let value = serde_json::to_value(&song).unwrap();
        assert_eq!(value["albumID"], "a");
        assert_eq!(value["trackNumber"], 2);

        let index = OwnIndex {
            version: INDEX_VERSION,
            songs_by_song_id: HashMap::from([(
                "s".to_owned(),
                OwnRecord {
                    song,
                    filename: "file.flac".to_owned(),
                    artwork_filename: String::new(),
                },
            )]),
        };
        let value = serde_json::to_value(index).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["songsBySongID"]["s"]["filename"], "file.flac");
        assert_eq!(value["songsBySongID"]["s"]["song"]["albumID"], "a");
        assert!(value["songsBySongID"]["s"].get("artworkFilename").is_none());
    }

    #[test]
    fn null_collections_are_read_as_empty_like_go() {
        let index: OwnIndex =
            serde_json::from_str(r#"{"version":1,"songsBySongID":null}"#).unwrap();
        assert!(index.songs_by_song_id.is_empty());

        let metadata: MetadataCache =
            serde_json::from_str(r#"{"version":1,"albums":null,"songsByAlbumID":null}"#).unwrap();
        assert!(metadata.albums.is_empty());
        assert!(metadata.songs_by_album_id.is_empty());

        let manifest: YoruneManifest =
            serde_json::from_str(r#"{"version":1,"filenamesBySongID":{},"songsBySongID":null}"#)
                .unwrap();
        assert!(manifest.filenames_by_song_id.unwrap().is_empty());
        assert!(manifest.songs_by_song_id.is_empty());

        let root = tempfile::tempdir().unwrap();
        let library = test_library(test_config(root.path()), root.path()).unwrap();
        fs::create_dir_all(library.cache_dir()).unwrap();
        fs::write(
            library.cache_dir().join("metadata.json"),
            r#"{"version":1,"albums":null,"songsByAlbumID":null}"#,
        )
        .unwrap();
        fs::write(&library.index_path, r#"{"version":1,"songsBySongID":null}"#).unwrap();
        let reopened = test_library(test_config(root.path()), root.path()).unwrap();
        assert!(reopened.downloaded_albums().is_empty());
        assert!(
            reopened
                .albums(false)
                .unwrap_err()
                .to_string()
                .contains("no music library")
        );
    }

    #[test]
    fn external_manifest_is_readable_and_remove_never_deletes_it() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("track.flac"), b"external audio").unwrap();
        fs::write(
            external.join("manifest.json"),
            r#"{
                "version": 1,
                "filenamesBySongID": {
                    "ok": "track.flac",
                    "escape": "../track.flac"
                },
                "songsBySongID": {
                    "ok": {
                        "title": "External",
                        "artist": "Artist",
                        "albumID": "album",
                        "albumTitle": "Album",
                        "duration": 10,
                        "trackNumber": 1
                    },
                    "escape": {
                        "albumID": "album"
                    }
                }
            }"#,
        )
        .unwrap();

        let mut cfg = test_config(root.path());
        cfg.storage.external_dirs = vec![external.to_string_lossy().into_owned()];
        let library = test_library(cfg, root.path()).unwrap();
        assert_eq!(library.downloaded_songs("album").len(), 1);
        assert_eq!(library.local_path("escape"), None);
        assert_eq!(library.local_path("ok"), Some(external.join("track.flac")));
        library.remove("ok").unwrap();
        assert!(external.join("track.flac").is_file());

        let temp = root.path().join("owned.flac");
        fs::write(&temp, b"owned audio").unwrap();
        library
            .save_download(&song("ok", "album", 1), &temp, "flac")
            .unwrap();
        assert!(library.is_owned("ok"));
        assert_ne!(
            library.local_path("ok").as_deref(),
            Some(external.join("track.flac").as_path())
        );
        library.remove("ok").unwrap();
        assert_eq!(library.local_path("ok"), Some(external.join("track.flac")));
        assert_eq!(
            fs::read(external.join("track.flac")).unwrap(),
            b"external audio"
        );
    }

    #[test]
    fn downloads_are_atomic_indexed_and_removable() {
        let root = tempfile::tempdir().unwrap();
        let library = test_library(test_config(root.path()), root.path()).unwrap();
        let temp = root.path().join("download.tmp");
        fs::write(&temp, b"first audio").unwrap();
        let track = song("song/id", "album", 1);

        library.save_download(&track, &temp, ".FLAC").unwrap();
        let path = library.local_path(&track.id).unwrap();
        assert_eq!(path.extension().unwrap(), "flac");
        assert_eq!(fs::read(&path).unwrap(), b"first audio");
        assert!(library.is_owned(&track.id));
        assert!(library.cache_dir().is_dir());

        fs::write(&temp, b"replacement audio").unwrap();
        library.save_download(&track, &temp, "mp3").unwrap();
        let replacement = library.local_path(&track.id).unwrap();
        assert_ne!(replacement, path);
        assert!(!path.exists());
        assert_eq!(replacement.extension().unwrap(), "mp3");
        assert_eq!(fs::read(&replacement).unwrap(), b"replacement audio");

        let reopened = test_library(test_config(root.path()), root.path()).unwrap();
        assert_eq!(reopened.downloaded_songs("album"), vec![track.clone()]);
        reopened.remove(&track.id).unwrap();
        assert_eq!(reopened.local_path(&track.id), None);
        assert!(reopened.downloaded_songs("").is_empty());
    }

    #[test]
    fn offline_library_uses_cached_metadata_and_single_album_response() {
        let root = tempfile::tempdir().unwrap();
        let library = test_library(test_config(root.path()), root.path()).unwrap();
        let snapshot = MetadataCache {
            version: METADATA_VERSION,
            albums: vec![CachedAlbum {
                id: "cached".to_owned(),
                title: "Cached Album".to_owned(),
                ..CachedAlbum::default()
            }],
            songs_by_album_id: HashMap::from([(
                "cached".to_owned(),
                vec![CachedSong {
                    id: "cached-song".to_owned(),
                    title: "Cached Song".to_owned(),
                    album_id: "cached".to_owned(),
                    ..CachedSong::default()
                }],
            )]),
        };
        fs::create_dir_all(library.cache_dir()).unwrap();
        fs::write(
            library.cache_dir().join("metadata.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let offline = test_library(test_config(root.path()), root.path()).unwrap();
        assert_eq!(offline.albums(false).unwrap()[0].id, "cached");
        assert_eq!(offline.songs("cached").unwrap()[0].id, "cached-song");

        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            let query = request.url().split_once('?').unwrap().1;
            assert!(query.contains("size=500"));
            assert!(query.contains("offset=0"));
            assert!(query.contains("type=alphabeticalByName"));
            request
                .respond(
                    Response::from_string(
                        r#"{"subsonic-response":{"status":"ok","albumList2":{"album":{"id":"remote","name":"Remote"}}}}"#,
                    )
                    .with_status_code(200),
                )
                .unwrap();
        });
        let mut cfg = test_config(root.path());
        cfg.server.url = format!("http://{address}");
        cfg.server.username = "user".to_owned();
        cfg.server.password = "password".to_owned();
        let remote = test_library(cfg, root.path()).unwrap();
        assert_eq!(remote.albums(true).unwrap()[0].id, "remote");
        handle.join().unwrap();
    }

    #[test]
    fn subsonic_auth_and_pagination_follow_go_protocol() {
        let root = tempfile::tempdir().unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let first_page: Vec<_> = (0..SUBSONIC_PAGE_SIZE)
            .map(|index| {
                serde_json::json!({
                    "id": format!("album-{index:03}"),
                    "name": format!("Album {index:03}")
                })
            })
            .collect();
        let first_body = serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "albumList2": { "album": first_page }
            }
        })
        .to_string();

        let handle = std::thread::spawn(move || {
            for (expected_offset, body) in [
                (0, first_body),
                (
                    500,
                    r#"{"subsonic-response":{"status":"ok","albumList2":{"album":{"id":"last","name":"Last"}}}}"#
                        .to_owned(),
                ),
            ] {
                let request = server.recv().unwrap();
                let url = Url::parse(&format!("http://test{}", request.url())).unwrap();
                let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(query["u"], "user");
                assert_eq!(query["s"].len(), 32);
                assert_eq!(
                    query["t"],
                    hex_encode(&md5::compute(format!("password{}", query["s"])).0)
                );
                assert_eq!(query["v"], "1.16.1");
                assert_eq!(query["c"], "Akari");
                assert_eq!(query["f"], "json");
                assert_eq!(query["type"], "alphabeticalByName");
                assert_eq!(query["size"], "500");
                assert_eq!(query["offset"], expected_offset.to_string());
                request
                    .respond(Response::from_string(body).with_status_code(200))
                    .unwrap();
            }
        });

        let mut cfg = test_config(root.path());
        cfg.server.url = format!("http://{address}/subsonic/?existing=kept#fragment");
        cfg.server.username = "user".to_owned();
        cfg.server.password = "password".to_owned();
        let library = test_library(cfg, root.path()).unwrap();
        let albums = library.albums(true).unwrap();
        assert_eq!(albums.len(), SUBSONIC_PAGE_SIZE + 1);
        assert_eq!(albums.last().unwrap().id, "last");

        let stream = Url::parse(&library.stream_url("song id").unwrap()).unwrap();
        assert_eq!(stream.path(), "/subsonic/rest/stream.view");
        assert_eq!(stream.fragment(), None);
        assert_eq!(
            stream.query_pairs().find(|(key, _)| key == "id").unwrap().1,
            "song id"
        );
        assert!(
            stream
                .query_pairs()
                .any(|(key, value)| key == "existing" && value == "kept")
        );
        handle.join().unwrap();
    }

    #[test]
    fn unsafe_download_extensions_are_rejected() {
        assert!(normalize_extension("../flac", Path::new("track")).is_err());
        assert!(normalize_extension("mp3/../../x", Path::new("track")).is_err());
        assert_eq!(
            normalize_extension("", Path::new("track.FLAC")).unwrap(),
            "flac"
        );
    }
}
