package library

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"

	"akari/internal/core"
)

const maxManifestSize = 64 << 20

type externalCatalog struct {
	songs        map[string]string
	songMetadata map[string]core.Song
	artwork      map[string]string
	albums       []core.Album
}

type yoruneManifest struct {
	Version                   int                   `json:"version"`
	FilenamesBySongID         map[string]string     `json:"filenamesBySongID"`
	SongsBySongID             map[string]yoruneSong `json:"songsBySongID"`
	ArtworkFilenamesByAlbumID map[string]string     `json:"artworkFilenamesByAlbumID"`
}

type yoruneSong struct {
	ID          string  `json:"id"`
	Title       string  `json:"title"`
	Artist      string  `json:"artist"`
	AlbumID     string  `json:"albumID"`
	AlbumTitle  string  `json:"albumTitle"`
	Duration    float64 `json:"duration"`
	TrackNumber *int    `json:"trackNumber"`
	DiscNumber  *int    `json:"discNumber"`
}

func (l *Library) reloadExternal() error {
	catalog := externalCatalog{
		songs:        make(map[string]string),
		songMetadata: make(map[string]core.Song),
		artwork:      make(map[string]string),
	}
	for _, configured := range l.config.Storage.ExternalDirs {
		root := strings.TrimSpace(configured)
		if root == "" {
			continue
		}
		for _, directory := range externalDirectories(root) {
			l.loadExternalDirectory(directory, &catalog)
		}
	}
	catalog.albums = albumsFromExternal(catalog.songMetadata)

	l.mu.Lock()
	l.external = catalog
	l.mu.Unlock()
	return nil
}

func externalDirectories(root string) []string {
	info, err := os.Stat(root)
	if err != nil || !info.IsDir() {
		return nil
	}
	if hasExternalIndex(root) {
		return []string{root}
	}

	entries, err := os.ReadDir(root)
	if err != nil {
		return nil
	}
	directories := make([]string, 0)
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		child := filepath.Join(root, entry.Name())
		if hasExternalIndex(child) {
			directories = append(directories, child)
		}
	}
	return directories
}

func hasExternalIndex(directory string) bool {
	return regularFile(filepath.Join(directory, "manifest.json")) ||
		regularFile(filepath.Join(directory, "index.json"))
}

func (l *Library) loadExternalDirectory(directory string, catalog *externalCatalog) {
	manifestPath := filepath.Join(directory, "manifest.json")
	if regularFile(manifestPath) {
		if data, err := readLimitedFile(manifestPath); err == nil {
			l.loadYoruneManifest(directory, data, catalog)
		}
		return
	}

	indexPath := filepath.Join(directory, "index.json")
	if data, err := readLimitedFile(indexPath); err == nil {
		l.loadExternalOwnIndex(directory, data, catalog)
	}
}

func (l *Library) loadYoruneManifest(directory string, data []byte, catalog *externalCatalog) {
	var manifest yoruneManifest
	if err := json.Unmarshal(data, &manifest); err != nil || manifest.FilenamesBySongID == nil {
		var legacy map[string]string
		if err := json.Unmarshal(data, &legacy); err != nil {
			return
		}
		manifest.FilenamesBySongID = legacy
	}

	for songID, filename := range manifest.FilenamesBySongID {
		if songID == "" || !safeFilename(filename) {
			continue
		}
		path := resolveExternalFile(directory, filename)
		if path == "" {
			continue
		}
		if _, exists := catalog.songs[songID]; !exists {
			catalog.songs[songID] = path
		}

		stored, ok := manifest.SongsBySongID[songID]
		if !ok {
			continue
		}
		song := core.Song{
			ID:          songID,
			Title:       stored.Title,
			Artist:      stored.Artist,
			AlbumID:     stored.AlbumID,
			AlbumTitle:  stored.AlbumTitle,
			Duration:    stored.Duration,
			TrackNumber: intValue(stored.TrackNumber),
			DiscNumber:  intValue(stored.DiscNumber),
		}
		if song.Title == "" {
			song.Title = songID
		}
		if _, exists := catalog.songMetadata[songID]; !exists {
			catalog.songMetadata[songID] = song
		}
	}

	for albumID, filename := range manifest.ArtworkFilenamesByAlbumID {
		if albumID == "" || !safeFilename(filename) {
			continue
		}
		path := resolveExternalFile(directory, filename)
		if path != "" {
			if _, exists := catalog.artwork[albumID]; !exists {
				catalog.artwork[albumID] = path
			}
		}
	}
}

func (l *Library) loadExternalOwnIndex(directory string, data []byte, catalog *externalCatalog) {
	var index ownIndex
	if err := json.Unmarshal(data, &index); err != nil {
		return
	}
	for songID, record := range index.SongsBySongID {
		if songID == "" || !safeFilename(record.Filename) {
			continue
		}
		path := resolveExternalFile(directory, record.Filename)
		if path == "" {
			continue
		}
		if _, exists := catalog.songs[songID]; !exists {
			catalog.songs[songID] = path
		}
		song := record.Song
		if song.ID == "" {
			song.ID = songID
		}
		if song.Title == "" {
			song.Title = songID
		}
		song.ArtworkURL = ""
		if _, exists := catalog.songMetadata[songID]; !exists {
			catalog.songMetadata[songID] = song
		}
		if record.ArtworkFilename != "" && safeFilename(record.ArtworkFilename) {
			artwork := resolveExternalFile(directory, record.ArtworkFilename)
			if artwork != "" {
				if _, exists := catalog.artwork[song.AlbumID]; !exists {
					catalog.artwork[song.AlbumID] = artwork
				}
			}
		}
	}
}

func albumsFromExternal(songs map[string]core.Song) []core.Album {
	byID := make(map[string]core.Album)
	for _, song := range songs {
		if song.AlbumID == "" {
			continue
		}
		if _, exists := byID[song.AlbumID]; exists {
			continue
		}
		title := song.AlbumTitle
		if title == "" {
			title = song.AlbumID
		}
		byID[song.AlbumID] = core.Album{
			ID:     song.AlbumID,
			Title:  title,
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

func readLimitedFile(path string) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	data, err := io.ReadAll(io.LimitReader(file, maxManifestSize+1))
	if err != nil {
		return nil, err
	}
	if len(data) > maxManifestSize {
		return nil, errors.New("manifest is too large")
	}
	return data, nil
}

func intValue(value *int) int {
	if value == nil {
		return 0
	}
	return *value
}

func resolveExternalFile(directory, filename string) string {
	if !safeFilename(filename) {
		return ""
	}
	for _, candidate := range []string{
		filepath.Join(directory, filename),
		filepath.Join(directory, "audio", filename),
		filepath.Join(directory, "artwork", filename),
	} {
		if regularFile(candidate) {
			return candidate
		}
	}
	return ""
}
