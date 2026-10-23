package library

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"akari/internal/core"
)

// SaveDownload atomically installs a completed download in Akari's namespaced
// music directory and updates its independent JSON index.
func (l *Library) SaveDownload(song core.Song, tempPath, extension string) error {
	if strings.TrimSpace(song.ID) == "" {
		return errors.New("song ID is empty")
	}
	extension, err := normalizeExtension(extension, tempPath)
	if err != nil {
		return err
	}
	sourceInfo, err := os.Stat(tempPath)
	if err != nil {
		return fmt.Errorf("open downloaded audio: %w", err)
	}
	if !sourceInfo.Mode().IsRegular() {
		return errors.New("downloaded audio is not a regular file")
	}

	stagedPath, err := copyDownload(tempPath, l.audioDir)
	if err != nil {
		return err
	}
	defer os.Remove(stagedPath)

	sum := sha256.Sum256([]byte(song.ID))
	filename := hex.EncodeToString(sum[:]) + "." + extension
	finalPath := filepath.Join(l.audioDir, filename)

	l.writeMu.Lock()
	defer l.writeMu.Unlock()

	l.mu.Lock()
	defer l.mu.Unlock()

	oldRecord, hadOldRecord := l.ownRecords[song.ID]
	backups := make(map[string]string)
	installed := false
	rollback := func() {
		if installed {
			_ = os.Remove(finalPath)
		}
		for path := range backups {
			_ = os.Remove(path)
		}
		for original, backup := range backups {
			_ = os.Rename(backup, original)
		}
		if hadOldRecord {
			l.ownRecords[song.ID] = oldRecord
		} else {
			delete(l.ownRecords, song.ID)
		}
	}

	pathsToBackup := []string{finalPath}
	if hadOldRecord {
		oldPath := filepath.Join(l.audioDir, oldRecord.Filename)
		if oldPath != finalPath {
			pathsToBackup = append(pathsToBackup, oldPath)
		}
	}
	for _, path := range pathsToBackup {
		if !regularFile(path) {
			continue
		}
		backup, err := backupFile(path)
		if err != nil {
			rollback()
			return fmt.Errorf("prepare download replacement: %w", err)
		}
		backups[path] = backup
	}

	if err := os.Rename(stagedPath, finalPath); err != nil {
		rollback()
		return fmt.Errorf("install downloaded audio: %w", err)
	}
	installed = true
	syncDirectory(l.audioDir)

	stored := song
	record := ownRecord{
		Song:     stored,
		Filename: filename,
	}
	l.ownRecords[song.ID] = record
	if err := l.persistIndexLocked(); err != nil {
		rollback()
		return err
	}

	for _, backup := range backups {
		_ = os.Remove(backup)
	}
	syncDirectory(l.audioDir)
	syncDirectory(filepath.Dir(l.indexPath))
	return nil
}

// Remove deletes a song only when it is tracked in Akari's own index. Files
// supplied through external manifest directories are never touched.
func (l *Library) Remove(songID string) error {
	l.writeMu.Lock()
	defer l.writeMu.Unlock()
	l.mu.Lock()
	defer l.mu.Unlock()

	record, ok := l.ownRecords[songID]
	if !ok {
		return nil
	}
	path := filepath.Join(l.audioDir, record.Filename)
	var backup string
	if regularFile(path) {
		var err error
		backup, err = backupFile(path)
		if err != nil {
			return fmt.Errorf("prepare downloaded file removal: %w", err)
		}
	}

	delete(l.ownRecords, songID)
	if err := l.persistIndexLocked(); err != nil {
		if ok {
			l.ownRecords[songID] = record
		}
		if backup != "" {
			_ = os.Rename(backup, path)
		}
		return err
	}
	if backup != "" {
		if err := os.Remove(backup); err != nil {
			return fmt.Errorf("remove downloaded audio: %w", err)
		}
	}

	if record.ArtworkFilename != "" && !albumArtworkInUse(l.ownRecords, record.Song.AlbumID) {
		artworkPath := filepath.Join(l.audioDir, record.ArtworkFilename)
		if regularFile(artworkPath) {
			if err := os.Remove(artworkPath); err != nil && !errors.Is(err, os.ErrNotExist) {
				return fmt.Errorf("remove album artwork: %w", err)
			}
		}
	}
	syncDirectory(l.audioDir)
	syncDirectory(filepath.Dir(l.indexPath))
	return nil
}

// DownloadedAlbums returns albums that have at least one readable local audio
// file, including read-only external library manifests.
func (l *Library) DownloadedAlbums() []core.Album {
	l.mu.RLock()
	defer l.mu.RUnlock()

	byID := make(map[string]core.Album)
	for _, album := range l.external.albums {
		if externalAlbumHasFile(l.external, album.ID) {
			byID[album.ID] = album
		}
	}
	for _, record := range l.ownRecords {
		song := record.Song
		if song.AlbumID == "" || !regularFile(filepath.Join(l.audioDir, record.Filename)) {
			continue
		}
		if _, exists := byID[song.AlbumID]; exists {
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

// DownloadedSongs returns readable local songs for an album. An empty albumID
// returns every local song.
func (l *Library) DownloadedSongs(albumID string) []core.Song {
	l.mu.RLock()
	defer l.mu.RUnlock()

	byID := make(map[string]core.Song)
	for songID, path := range l.external.songs {
		if !regularFile(path) {
			continue
		}
		song, ok := l.external.songMetadata[songID]
		if !ok || (albumID != "" && song.AlbumID != albumID) {
			continue
		}
		byID[songID] = song
	}
	for songID, record := range l.ownRecords {
		if albumID != "" && record.Song.AlbumID != albumID {
			continue
		}
		if !regularFile(filepath.Join(l.audioDir, record.Filename)) {
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

func (l *Library) persistIndexLocked() error {
	index := ownIndex{
		Version:       indexVersion,
		SongsBySongID: make(map[string]ownRecord, len(l.ownRecords)),
	}
	for songID, record := range l.ownRecords {
		index.SongsBySongID[songID] = record
	}
	data, err := json.Marshal(index)
	if err != nil {
		return fmt.Errorf("encode download index: %w", err)
	}
	if err := writeFileAtomic(l.indexPath, data, 0o600); err != nil {
		return fmt.Errorf("write download index: %w", err)
	}
	return nil
}

func copyDownload(sourcePath, directory string) (string, error) {
	source, err := os.Open(sourcePath)
	if err != nil {
		return "", fmt.Errorf("open downloaded audio: %w", err)
	}
	defer source.Close()

	target, err := os.CreateTemp(directory, ".akari-audio-*")
	if err != nil {
		return "", fmt.Errorf("create temporary download: %w", err)
	}
	targetPath := target.Name()
	committed := false
	defer func() {
		_ = target.Close()
		if !committed {
			_ = os.Remove(targetPath)
		}
	}()

	if err := target.Chmod(0o600); err != nil {
		return "", fmt.Errorf("set download permissions: %w", err)
	}
	if _, err := io.Copy(target, source); err != nil {
		return "", fmt.Errorf("copy downloaded audio: %w", err)
	}
	if err := target.Sync(); err != nil {
		return "", fmt.Errorf("sync downloaded audio: %w", err)
	}
	if err := target.Close(); err != nil {
		return "", fmt.Errorf("close downloaded audio: %w", err)
	}
	committed = true
	return targetPath, nil
}

func backupFile(path string) (string, error) {
	target, err := os.CreateTemp(filepath.Dir(path), ".akari-backup-*")
	if err != nil {
		return "", err
	}
	backup := target.Name()
	if err := target.Close(); err != nil {
		_ = os.Remove(backup)
		return "", err
	}
	if err := os.Remove(backup); err != nil {
		return "", err
	}
	if err := os.Rename(path, backup); err != nil {
		return "", err
	}
	return backup, nil
}

func normalizeExtension(extension, tempPath string) (string, error) {
	extension = strings.TrimSpace(extension)
	if extension == "" {
		extension = filepath.Ext(tempPath)
	}
	extension = strings.TrimLeft(extension, ".")
	if extension == "" {
		return "audio", nil
	}
	if len(extension) > 10 {
		return "", errors.New("download extension is too long")
	}
	for _, char := range extension {
		if (char < 'a' || char > 'z') &&
			(char < 'A' || char > 'Z') &&
			(char < '0' || char > '9') {
			return "", fmt.Errorf("download extension %q is invalid", extension)
		}
	}
	return strings.ToLower(extension), nil
}

func albumArtworkInUse(records map[string]ownRecord, albumID string) bool {
	for _, record := range records {
		if record.Song.AlbumID == albumID && record.ArtworkFilename != "" {
			return true
		}
	}
	return false
}

func externalAlbumHasFile(catalog externalCatalog, albumID string) bool {
	for songID := range catalog.songs {
		song, ok := catalog.songMetadata[songID]
		if ok && song.AlbumID == albumID && regularFile(catalog.songs[songID]) {
			return true
		}
	}
	return false
}
