package library

import (
	"context"
	"crypto/md5"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"akari/internal/core"
)

const (
	indexVersion    = 1
	metadataVersion = 1
)

// Library provides Akari's remote catalog and local, download-backed catalog.
// All exported methods are safe for concurrent use.
type Library struct {
	config       core.Config
	namespace    string
	musicDir     string
	audioDir     string
	cacheDir     string
	indexPath    string
	metadataPath string
	client       *http.Client
	lifetimeCtx  context.Context
	cancel       context.CancelFunc
	closeOnce    sync.Once

	authSalt  string
	authToken string

	mu         sync.RWMutex
	metadataMu sync.Mutex
	writeMu    sync.Mutex
	albums     []cachedAlbum
	songs      map[string][]cachedSong
	metadataOK bool
	ownRecords map[string]ownRecord
	external   externalCatalog
}

type ownIndex struct {
	Version       int                  `json:"version"`
	SongsBySongID map[string]ownRecord `json:"songsBySongID"`
}

type ownRecord struct {
	Song            core.Song `json:"song"`
	Filename        string    `json:"filename"`
	ArtworkFilename string    `json:"artworkFilename,omitempty"`
}

type metadataCache struct {
	Version        int                     `json:"version"`
	Albums         []cachedAlbum           `json:"albums"`
	SongsByAlbumID map[string][]cachedSong `json:"songsByAlbumID"`
}

type cachedAlbum struct {
	ID         string  `json:"id"`
	Title      string  `json:"title"`
	Artist     string  `json:"artist,omitempty"`
	Genre      string  `json:"genre,omitempty"`
	Year       int     `json:"year,omitempty"`
	LastPlayed float64 `json:"lastPlayed,omitempty"`
}

type cachedSong struct {
	ID          string  `json:"id"`
	Title       string  `json:"title"`
	Artist      string  `json:"artist,omitempty"`
	AlbumID     string  `json:"albumID"`
	AlbumTitle  string  `json:"albumTitle,omitempty"`
	Duration    float64 `json:"duration,omitempty"`
	TrackNumber int     `json:"trackNumber,omitempty"`
	DiscNumber  int     `json:"discNumber,omitempty"`
}

// New constructs a library and loads local metadata and download state.
func New(cfg core.Config) (*Library, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return nil, fmt.Errorf("determine home directory: %w", err)
	}

	musicDir := strings.TrimSpace(cfg.Storage.MusicDir)
	if musicDir == "" {
		musicDir = defaultMusicDir(home)
	} else {
		musicDir = expandHome(musicDir, home)
	}
	musicDir, err = filepath.Abs(musicDir)
	if err != nil {
		return nil, fmt.Errorf("resolve music directory: %w", err)
	}
	cfg.Storage.MusicDir = musicDir
	externalDirs := append([]string(nil), cfg.Storage.ExternalDirs...)
	for i, dir := range externalDirs {
		externalDirs[i] = expandHome(dir, home)
	}
	cfg.Storage.ExternalDirs = externalDirs

	namespace := namespaceFor(cfg.Server.URL, cfg.Server.Username)
	cacheRoot := os.Getenv("XDG_CACHE_HOME")
	if cacheRoot == "" {
		cacheRoot = filepath.Join(home, ".cache")
	} else {
		cacheRoot = expandHome(cacheRoot, home)
	}
	cacheDir, err := filepath.Abs(filepath.Join(cacheRoot, "akari", namespace))
	if err != nil {
		return nil, fmt.Errorf("resolve cache directory: %w", err)
	}
	audioDir := filepath.Join(musicDir, namespace, "audio")
	namespaceDir := filepath.Join(musicDir, namespace)

	if err := os.MkdirAll(audioDir, 0o700); err != nil {
		return nil, fmt.Errorf("create music directory: %w", err)
	}
	if err := os.MkdirAll(cacheDir, 0o700); err != nil {
		return nil, fmt.Errorf("create cache directory: %w", err)
	}

	saltBytes := make([]byte, 16)
	if _, err := rand.Read(saltBytes); err != nil {
		return nil, fmt.Errorf("initialize server authentication: %w", err)
	}
	salt := hex.EncodeToString(saltBytes)
	tokenBytes := md5.Sum([]byte(cfg.Server.Password + salt))
	lifetimeCtx, cancel := context.WithCancel(context.Background())

	lib := &Library{
		config:       cfg,
		namespace:    namespace,
		musicDir:     musicDir,
		audioDir:     audioDir,
		cacheDir:     cacheDir,
		indexPath:    filepath.Join(namespaceDir, "index.json"),
		metadataPath: filepath.Join(cacheDir, "metadata.json"),
		client: &http.Client{
			Timeout: 60 * time.Second,
			Transport: &lifetimeTransport{
				lifetime: lifetimeCtx,
				base:     http.DefaultTransport,
			},
		},
		lifetimeCtx: lifetimeCtx,
		cancel:      cancel,
		authSalt:    salt,
		authToken:   hex.EncodeToString(tokenBytes[:]),
		songs:       make(map[string][]cachedSong),
		ownRecords:  make(map[string]ownRecord),
	}

	if err := lib.loadIndex(); err != nil {
		cancel()
		return nil, err
	}
	lib.loadMetadata()
	if err := lib.reloadExternal(); err != nil {
		cancel()
		return nil, err
	}
	return lib, nil
}

// Albums returns the remote catalog when needed and falls back to cached and
// local metadata when the server cannot be reached.
func (l *Library) Albums(ctx context.Context, refresh bool) ([]core.Album, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	_ = l.reloadExternal()

	l.mu.RLock()
	haveCache := l.metadataOK
	cachedAlbums := append([]cachedAlbum(nil), l.albums...)
	l.mu.RUnlock()

	var remoteErr error
	if refresh || !haveCache {
		if !l.hasServer() {
			remoteErr = errNoServer
		} else {
			albums, err := l.fetchAlbums(ctx)
			if err == nil {
				cachedAlbums = albums
				if cacheErr := l.storeMetadataAlbums(albums); cacheErr != nil {
					remoteErr = cacheErr
				}
			} else {
				remoteErr = err
			}
		}
	}

	combined := l.combinedAlbums(cachedAlbums)
	if len(combined) > 0 {
		if remoteErr != nil && !errors.Is(remoteErr, errNoServer) {
			return combined, fmt.Errorf("Offline: %w", remoteErr)
		}
		return combined, nil
	}
	if remoteErr != nil && !errors.Is(remoteErr, errNoServer) {
		return nil, fmt.Errorf("load albums: %w", remoteErr)
	}
	return nil, errors.New("no music library is available; configure [server] or add a cached or external library")
}

// Songs returns cached songs when available and fetches one album from the
// server on a cache miss. Local downloads remain available without a server.
func (l *Library) Songs(ctx context.Context, albumID string) ([]core.Song, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	if strings.TrimSpace(albumID) == "" {
		return nil, errors.New("album ID is empty")
	}

	if err := l.reloadExternal(); err != nil {
		return nil, err
	}

	l.mu.RLock()
	cached, cachedOK := l.songs[albumID]
	l.mu.RUnlock()

	var remoteErr error
	albumSongs := cached
	if !cachedOK {
		if !l.hasServer() {
			remoteErr = errNoServer
		} else {
			fetched, album, err := l.fetchSongs(ctx, albumID)
			if err == nil {
				albumSongs = fetched
				if cacheErr := l.storeMetadataSongs(albumID, fetched, album); cacheErr != nil {
					remoteErr = cacheErr
				}
			} else {
				remoteErr = err
			}
		}
	}

	l.mu.RLock()
	local := append([]cachedSong(nil), l.songs[albumID]...)
	l.mu.RUnlock()
	combined := l.combinedSongs(albumID, albumSongs, local)
	if len(combined) > 0 {
		return combined, nil
	}
	if remoteErr != nil && !errors.Is(remoteErr, errNoServer) {
		return nil, fmt.Errorf("load songs for album %q: %w", albumID, remoteErr)
	}
	return nil, fmt.Errorf("no songs are available for album %q", albumID)
}

// LocalPath returns the downloaded audio path for a song, or an empty string.
func (l *Library) LocalPath(songID string) string {
	l.mu.RLock()
	defer l.mu.RUnlock()

	if record, ok := l.ownRecords[songID]; ok {
		path := filepath.Join(l.audioDir, record.Filename)
		if regularFile(path) {
			return path
		}
	}
	if path := l.external.songs[songID]; path != "" && regularFile(path) {
		return path
	}
	return ""
}

// StreamURL returns a token-authenticated Subsonic stream URL.
func (l *Library) StreamURL(songID string) string {
	if strings.TrimSpace(songID) == "" {
		return ""
	}
	url, err := l.endpoint("stream", map[string]string{
		"id":     songID,
		"format": "raw",
	})
	if err != nil {
		return ""
	}
	return url
}

// DownloadURL returns a token-authenticated Subsonic download URL.
func (l *Library) DownloadURL(songID string) string {
	if strings.TrimSpace(songID) == "" {
		return ""
	}
	url, err := l.endpoint("download", map[string]string{
		"id":     songID,
		"format": "raw",
	})
	if err != nil {
		return ""
	}
	return url
}

// IsOwned reports whether Akari itself manages the song's downloaded file.
func (l *Library) IsOwned(songID string) bool {
	l.mu.RLock()
	defer l.mu.RUnlock()
	record, ok := l.ownRecords[songID]
	if !ok {
		return false
	}
	return regularFile(filepath.Join(l.audioDir, record.Filename))
}

// CacheDir returns the namespaced directory containing cached remote metadata.
func (l *Library) CacheDir() string {
	return l.cacheDir
}

// HTTPClient returns the client used for Subsonic metadata and downloads.
func (l *Library) HTTPClient() *http.Client {
	return l.client
}

// Close cancels in-flight HTTP requests and releases idle connections.
func (l *Library) Close() error {
	if l == nil {
		return nil
	}
	l.closeOnce.Do(func() {
		if l.cancel != nil {
			l.cancel()
		}
		if l.client != nil {
			l.client.CloseIdleConnections()
		}
	})
	return nil
}

func namespaceFor(serverURL, username string) string {
	sum := sha256.Sum256([]byte(serverURL + "\n" + username))
	return hex.EncodeToString(sum[:])
}

func (l *Library) hasServer() bool {
	return strings.TrimSpace(l.config.Server.URL) != ""
}

func (l *Library) loadIndex() error {
	data, err := os.ReadFile(l.indexPath)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("read download index: %w", err)
	}
	return l.decodeIndex(data)
}

func (l *Library) decodeIndex(data []byte) error {
	var index ownIndex
	if err := json.Unmarshal(data, &index); err != nil {
		return fmt.Errorf("parse download index %q: invalid JSON", l.indexPath)
	}
	if index.Version != 0 && index.Version != indexVersion {
		return fmt.Errorf("parse download index %q: unsupported version %d", l.indexPath, index.Version)
	}
	if index.SongsBySongID == nil {
		index.SongsBySongID = make(map[string]ownRecord)
	}

	for songID, record := range index.SongsBySongID {
		if !safeFilename(record.Filename) {
			continue
		}
		if record.Song.ID == "" {
			record.Song.ID = songID
		}
		index.SongsBySongID[songID] = record
	}
	l.ownRecords = index.SongsBySongID
	return nil
}

func (l *Library) loadMetadata() {
	data, err := os.ReadFile(l.metadataPath)
	if err != nil {
		return
	}
	var cache metadataCache
	if err := json.Unmarshal(data, &cache); err != nil {
		return
	}
	if cache.Version != 0 && cache.Version != metadataVersion {
		return
	}
	if cache.SongsByAlbumID == nil {
		cache.SongsByAlbumID = make(map[string][]cachedSong)
	}

	l.mu.Lock()
	l.albums = cache.Albums
	l.songs = cache.SongsByAlbumID
	l.metadataOK = true
	l.mu.Unlock()
}

func (l *Library) storeMetadataAlbums(albums []cachedAlbum) error {
	l.metadataMu.Lock()
	defer l.metadataMu.Unlock()

	l.mu.Lock()
	l.albums = append([]cachedAlbum(nil), albums...)
	l.metadataOK = true
	snapshot := l.metadataSnapshotLocked()
	l.mu.Unlock()

	data, err := json.Marshal(snapshot)
	if err != nil {
		return fmt.Errorf("encode album cache: %w", err)
	}
	if err := writeFileAtomic(l.metadataPath, data, 0o600); err != nil {
		return fmt.Errorf("write album cache: %w", err)
	}
	return nil
}

func (l *Library) storeMetadataSongs(albumID string, songs []cachedSong, album *cachedAlbum) error {
	l.metadataMu.Lock()
	defer l.metadataMu.Unlock()

	l.mu.Lock()
	l.songs[albumID] = append([]cachedSong(nil), songs...)
	l.metadataOK = true
	if album != nil && !containsCachedAlbum(l.albums, album.ID) {
		l.albums = append(l.albums, *album)
	}
	snapshot := l.metadataSnapshotLocked()
	l.mu.Unlock()

	data, err := json.Marshal(snapshot)
	if err != nil {
		return fmt.Errorf("encode song cache: %w", err)
	}
	if err := writeFileAtomic(l.metadataPath, data, 0o600); err != nil {
		return fmt.Errorf("write song cache: %w", err)
	}
	return nil
}

func (l *Library) metadataSnapshotLocked() metadataCache {
	songs := make(map[string][]cachedSong, len(l.songs))
	for albumID, albumSongs := range l.songs {
		songs[albumID] = append([]cachedSong(nil), albumSongs...)
	}
	return metadataCache{
		Version:        metadataVersion,
		Albums:         append([]cachedAlbum(nil), l.albums...),
		SongsByAlbumID: songs,
	}
}

func (l *Library) combinedAlbums(cachedAlbums []cachedAlbum) []core.Album {
	l.mu.RLock()
	defer l.mu.RUnlock()

	byID := make(map[string]core.Album)
	for _, album := range cachedAlbums {
		item := l.remoteAlbum(album)
		byID[item.ID] = item
	}
	for _, album := range l.external.albums {
		if _, ok := byID[album.ID]; !ok {
			byID[album.ID] = album
		}
	}
	for _, record := range l.ownRecords {
		song := record.Song
		if song.AlbumID == "" {
			continue
		}
		if _, ok := byID[song.AlbumID]; ok {
			continue
		}
		byID[song.AlbumID] = core.Album{
			ID:     song.AlbumID,
			Title:  song.AlbumTitle,
			Artist: song.Artist,
		}
	}

	albums := make([]core.Album, 0, len(byID))
	for _, album := range byID {
		albums = append(albums, album)
	}
	sortAlbums(albums)
	return albums
}

func (l *Library) combinedSongs(albumID string, candidates, cached []cachedSong) []core.Song {
	l.mu.RLock()
	defer l.mu.RUnlock()

	byID := make(map[string]core.Song)
	for _, song := range candidates {
		item := l.remoteSong(song)
		byID[item.ID] = item
	}
	for _, song := range cached {
		item := l.remoteSong(song)
		byID[item.ID] = item
	}
	for songID, path := range l.external.songs {
		if !regularFile(path) {
			continue
		}
		song, ok := l.external.songMetadata[songID]
		if !ok || song.AlbumID != albumID {
			continue
		}
		byID[songID] = song
	}
	for songID, record := range l.ownRecords {
		if record.Song.AlbumID != albumID || !regularFile(filepath.Join(l.audioDir, record.Filename)) {
			continue
		}
		song := record.Song
		byID[songID] = song
	}

	songs := make([]core.Song, 0, len(byID))
	for _, song := range byID {
		songs = append(songs, song)
	}
	sortSongs(songs)
	return songs
}

func (l *Library) remoteAlbum(album cachedAlbum) core.Album {
	return core.Album{
		ID:         album.ID,
		Title:      album.Title,
		Artist:     album.Artist,
		Genre:      album.Genre,
		Year:       album.Year,
		LastPlayed: album.LastPlayed,
	}
}

func (l *Library) remoteSong(song cachedSong) core.Song {
	return core.Song{
		ID:          song.ID,
		Title:       song.Title,
		Artist:      song.Artist,
		AlbumID:     song.AlbumID,
		AlbumTitle:  song.AlbumTitle,
		Duration:    song.Duration,
		TrackNumber: song.TrackNumber,
		DiscNumber:  song.DiscNumber,
	}
}

func sortAlbums(albums []core.Album) {
	sort.SliceStable(albums, func(i, j int) bool {
		left := strings.ToLower(strings.TrimSpace(albums[i].Title))
		right := strings.ToLower(strings.TrimSpace(albums[j].Title))
		if left != right {
			return left < right
		}
		return albums[i].ID < albums[j].ID
	})
}

func sortSongs(songs []core.Song) {
	sort.SliceStable(songs, func(i, j int) bool {
		leftDisc := songs[i].DiscNumber
		rightDisc := songs[j].DiscNumber
		if leftDisc == 0 {
			leftDisc = 1
		}
		if rightDisc == 0 {
			rightDisc = 1
		}
		if leftDisc != rightDisc {
			return leftDisc < rightDisc
		}
		if songs[i].TrackNumber != songs[j].TrackNumber {
			if songs[i].TrackNumber == 0 {
				return false
			}
			if songs[j].TrackNumber == 0 {
				return true
			}
			return songs[i].TrackNumber < songs[j].TrackNumber
		}
		leftTitle := strings.ToLower(strings.TrimSpace(songs[i].Title))
		rightTitle := strings.ToLower(strings.TrimSpace(songs[j].Title))
		if leftTitle != rightTitle {
			return leftTitle < rightTitle
		}
		return songs[i].ID < songs[j].ID
	})
}

func containsCachedAlbum(albums []cachedAlbum, albumID string) bool {
	for _, album := range albums {
		if album.ID == albumID {
			return true
		}
	}
	return false
}

func safeFilename(value string) bool {
	return value != "" &&
		value != "." &&
		value != ".." &&
		filepath.Base(value) == value &&
		!strings.ContainsAny(value, `/\`)
}

func regularFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

func writeFileAtomic(path string, data []byte, mode os.FileMode) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	temp, err := os.CreateTemp(dir, ".akari-json-*")
	if err != nil {
		return err
	}
	tempPath := temp.Name()
	committed := false
	defer func() {
		_ = temp.Close()
		if !committed {
			_ = os.Remove(tempPath)
		}
	}()

	if err := temp.Chmod(mode); err != nil {
		return err
	}
	if _, err := temp.Write(data); err != nil {
		return err
	}
	if err := temp.Sync(); err != nil {
		return err
	}
	if err := temp.Close(); err != nil {
		return err
	}
	if err := os.Rename(tempPath, path); err != nil {
		return err
	}
	committed = true
	syncDirectory(dir)
	return nil
}

func syncDirectory(path string) {
	dir, err := os.Open(path)
	if err != nil {
		return
	}
	_ = dir.Sync()
	_ = dir.Close()
}

type lifetimeTransport struct {
	lifetime context.Context
	base     http.RoundTripper
}

func (transport *lifetimeTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	if err := transport.lifetime.Err(); err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(request.Context())
	stop := context.AfterFunc(transport.lifetime, cancel)
	response, err := transport.base.RoundTrip(request.Clone(ctx))
	if err != nil {
		stop()
		cancel()
		return nil, err
	}
	if response.Body == nil {
		stop()
		cancel()
		return response, nil
	}
	response.Body = &lifetimeBody{
		ReadCloser: response.Body,
		cancel:     cancel,
		stop:       stop,
	}
	return response, nil
}

type lifetimeBody struct {
	io.ReadCloser
	once   sync.Once
	cancel context.CancelFunc
	stop   func() bool
}

func (body *lifetimeBody) Close() error {
	err := body.ReadCloser.Close()
	body.once.Do(func() {
		body.stop()
		body.cancel()
	})
	return err
}

var errNoServer = errors.New("no server is configured")

func (transport *lifetimeTransport) CloseIdleConnections() {
	if c, ok := transport.base.(interface{ CloseIdleConnections() }); ok {
		c.CloseIdleConnections()
	}
}
