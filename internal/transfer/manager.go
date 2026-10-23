package transfer

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"sort"
	"sync"
	"time"

	"akari/internal/core"
	"akari/internal/library"
)

const (
	StateIdle        = "idle"
	StateQueued      = "queued"
	StateDownloading = "downloading"
	StateStreaming   = "streaming"
	StateSaving      = "saving"
	StateComplete    = "complete"
	StateError       = "error"
	StateCanceled    = "canceled"
)

const manualConcurrency = 3

type Status struct {
	Song  core.Song
	Bytes int64
	Total int64
	State string
	Error string
}

// Manager serves lazy playback from a loopback cache and owns manual downloads.
type Manager struct {
	lib     *library.Library
	client  *http.Client
	baseURL string
	tempDir string

	ctx    context.Context
	cancel context.CancelFunc

	server   *http.Server
	listener net.Listener

	mu          sync.Mutex
	entries     map[string]*entry
	closed      bool
	manualSlots chan struct{}
	handlerWG   sync.WaitGroup
	wg          sync.WaitGroup

	closeOnce sync.Once
	closeErr  error
}

type playbackRun struct {
	token     string
	sourceURL string
	ctx       context.Context
	cancel    context.CancelFunc
	leases    int
	handlers  int
}

type manualJob struct {
	sourceURL string
	ctx       context.Context
	cancel    context.CancelFunc
	canceled  bool
}

type entry struct {
	id        string
	cachePath string

	fetch chan struct{}

	mu               sync.Mutex
	song             core.Song
	total            int64
	ranges           []byteRange
	contentType      string
	extension        string
	disposition      string
	playback         *playbackRun
	manualRetained   bool
	manualJob        *manualJob
	manualWorkers    int
	activeHandlers   int
	discardRequested bool
	state            string
	lastErr          string
	saving           bool
	saved            bool
	saveAttempted    bool
}

// New starts a loopback cache proxy bound to a random IPv4 port.
func New(lib *library.Library) (*Manager, error) {
	if lib == nil {
		return nil, errors.New("transfer: library is nil")
	}

	tempDir, err := os.MkdirTemp("", "akari-transfer-*")
	if err != nil {
		return nil, errors.New("transfer: create playback cache directory")
	}

	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		_ = os.RemoveAll(tempDir)
		return nil, errors.New("transfer: listen on loopback")
	}

	ctx, cancel := context.WithCancel(context.Background())
	client := lib.HTTPClient()
	if client == nil {
		client = http.DefaultClient
	} else {
		// The library client's overall timeout would truncate long playback
		// streams. Per-request cancellation still comes from transfer contexts.
		copied := *client
		copied.Timeout = 0
		client = &copied
	}

	addr, ok := listener.Addr().(*net.TCPAddr)
	if !ok {
		cancel()
		_ = listener.Close()
		_ = os.RemoveAll(tempDir)
		return nil, errors.New("transfer: invalid loopback address")
	}

	m := &Manager{
		lib:         lib,
		client:      client,
		baseURL:     fmt.Sprintf("http://127.0.0.1:%d", addr.Port),
		tempDir:     tempDir,
		ctx:         ctx,
		cancel:      cancel,
		listener:    listener,
		entries:     make(map[string]*entry),
		manualSlots: make(chan struct{}, manualConcurrency),
	}
	m.server = &http.Server{
		Handler:           m,
		ReadHeaderTimeout: defaultReadHeaderTimeout,
		MaxHeaderBytes:    64 << 10,
		ErrorLog:          log.New(io.Discard, "", 0),
	}

	m.wg.Add(1)
	go func() {
		defer m.wg.Done()
		_ = m.server.Serve(listener)
	}()

	return m, nil
}

// Source returns an existing local file or a lazy loopback playback URL.
func (m *Manager) Source(song core.Song) (string, error) {
	if song.ID == "" {
		return "", errors.New("transfer: song ID is empty")
	}
	if m.isClosed() {
		return "", errors.New("transfer: manager is closed")
	}
	if local := m.localPath(song.ID); local != "" {
		return local, nil
	}

	streamURL, err := rawURL(m.lib.StreamURL(song.ID))
	if err != nil {
		return "", err
	}
	token, err := randomToken()
	if err != nil {
		return "", errors.New("transfer: create playback token")
	}

	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return "", errors.New("transfer: manager is closed")
	}
	e := m.ensureEntryLocked(song)
	e.mu.Lock()
	e.song = song
	e.discardRequested = false
	if e.playback == nil {
		ctx, cancel := context.WithCancel(m.ctx)
		e.playback = &playbackRun{
			token:     token,
			sourceURL: streamURL,
			ctx:       ctx,
			cancel:    cancel,
		}
	}
	e.playback.leases++
	playbackToken := e.playback.token
	e.mu.Unlock()
	m.mu.Unlock()

	// A save may have completed between the first check and registration.
	if local := m.localPath(song.ID); local != "" {
		e.mu.Lock()
		if run := e.playback; run != nil && run.token == playbackToken {
			if run.leases > 0 {
				run.leases--
			}
			if run.leases == 0 {
				run.cancel()
				if e.playback == run {
					e.playback = nil
				}
			}
		}
		e.mu.Unlock()
		m.reconcile(e)
		return local, nil
	}

	values := url.Values{}
	values.Set("id", song.ID)
	values.Set("token", playbackToken)
	return m.baseURL + "/stream?" + values.Encode(), nil
}

// Download retains the song and starts a manual raw download.
func (m *Manager) Download(song core.Song) error {
	if song.ID == "" {
		return errors.New("transfer: song ID is empty")
	}
	if m.isClosed() {
		return errors.New("transfer: manager is closed")
	}
	if local := m.localPath(song.ID); local != "" {
		return nil
	}

	downloadURL, err := rawURL(m.lib.DownloadURL(song.ID))
	if err != nil {
		return err
	}

	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return errors.New("transfer: manager is closed")
	}
	e := m.ensureEntryLocked(song)
	e.mu.Lock()
	e.song = song
	e.manualRetained = true
	e.discardRequested = false
	e.lastErr = ""
	e.saveAttempted = false

	if e.manualJob != nil && !e.manualJob.canceled {
		e.mu.Unlock()
		m.mu.Unlock()
		return nil
	}

	if e.isFullLocked() {
		e.state = StateSaving
		e.mu.Unlock()
		m.mu.Unlock()
		m.maybeSave(e)
		return nil
	}

	ctx, cancel := context.WithCancel(m.ctx)
	job := &manualJob{
		sourceURL: downloadURL,
		ctx:       ctx,
		cancel:    cancel,
	}
	e.manualJob = job
	e.manualWorkers++
	e.state = StateQueued
	m.wg.Add(1)
	e.mu.Unlock()
	m.mu.Unlock()

	go m.runManual(e, job)
	return nil
}

// Cancel ends the retained manual download. Active playback is left running.
func (m *Manager) Cancel(songID string) {
	m.mu.Lock()
	e := m.entries[songID]
	m.mu.Unlock()
	if e == nil {
		return
	}

	e.mu.Lock()
	e.manualRetained = false
	e.discardRequested = true
	e.lastErr = ""
	if e.manualJob != nil {
		e.manualJob.canceled = true
		e.manualJob.cancel()
	}
	if e.state == StateQueued || e.state == StateDownloading {
		e.state = StateCanceled
	}
	e.mu.Unlock()

	m.reconcile(e)
}

// Release drops non-retained playback state. A retained manual download continues.
func (m *Manager) Release(songID string) {
	m.mu.Lock()
	e := m.entries[songID]
	m.mu.Unlock()
	if e == nil {
		return
	}

	e.mu.Lock()
	if e.playback != nil {
		e.playback.cancel()
		e.playback = nil
	}
	if e.manualRetained {
		e.mu.Unlock()
		m.maybeSave(e)
		return
	}

	e.discardRequested = true
	if e.manualJob != nil {
		e.manualJob.canceled = true
		e.manualJob.cancel()
	}
	e.mu.Unlock()

	m.reconcile(e)
}

// Snapshot returns a stable, sorted view of cached and manual transfers.
func (m *Manager) Snapshot() []Status {
	m.mu.Lock()
	entries := make([]*entry, 0, len(m.entries))
	for _, e := range m.entries {
		entries = append(entries, e)
	}
	m.mu.Unlock()

	statuses := make([]Status, 0, len(entries))
	for _, e := range entries {
		e.mu.Lock()
		statuses = append(statuses, e.statusLocked())
		e.mu.Unlock()
	}
	sort.Slice(statuses, func(i, j int) bool {
		if statuses[i].Song.Artist != statuses[j].Song.Artist {
			return statuses[i].Song.Artist < statuses[j].Song.Artist
		}
		if statuses[i].Song.AlbumTitle != statuses[j].Song.AlbumTitle {
			return statuses[i].Song.AlbumTitle < statuses[j].Song.AlbumTitle
		}
		if statuses[i].Song.TrackNumber != statuses[j].Song.TrackNumber {
			return statuses[i].Song.TrackNumber < statuses[j].Song.TrackNumber
		}
		return statuses[i].Song.ID < statuses[j].Song.ID
	})
	return statuses
}

// Close cancels transfers, stops the proxy, removes partial files, and waits.
func (m *Manager) Close() error {
	m.closeOnce.Do(func() {
		m.mu.Lock()
		m.closed = true
		entries := make([]*entry, 0, len(m.entries))
		for _, e := range m.entries {
			entries = append(entries, e)
		}
		m.mu.Unlock()

		m.cancel()
		for _, e := range entries {
			e.mu.Lock()
			if e.playback != nil {
				e.playback.cancel()
				e.playback = nil
			}
			if e.manualJob != nil {
				e.manualJob.canceled = true
				e.manualJob.cancel()
			}
			e.mu.Unlock()
		}

		shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 5*time.Second)
		shutdownErr := m.server.Shutdown(shutdownCtx)
		shutdownCancel()
		if shutdownErr != nil {
			_ = m.server.Close()
		}
		m.handlerWG.Wait()
		m.wg.Wait()
		removeErr := os.RemoveAll(m.tempDir)

		switch {
		case shutdownErr != nil:
			m.closeErr = errors.New("transfer: close proxy timed out")
		case removeErr != nil:
			m.closeErr = errors.New("transfer: remove playback cache")
		}
	})
	return m.closeErr
}

func (m *Manager) ensureEntryLocked(song core.Song) *entry {
	if e := m.entries[song.ID]; e != nil {
		return e
	}
	e := &entry{
		id:        song.ID,
		cachePath: m.tempDir + string(os.PathSeparator) + cacheFileName(song.ID),
		fetch:     make(chan struct{}, 1),
		song:      song,
		total:     -1,
		state:     StateIdle,
	}
	m.entries[song.ID] = e
	return e
}

func (m *Manager) localPath(songID string) string {
	path := m.lib.LocalPath(songID)
	if path == "" {
		return ""
	}
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
		return ""
	}
	return path
}

func (m *Manager) isClosed() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.closed
}

func (m *Manager) runManual(e *entry, job *manualJob) {
	defer m.wg.Done()

	queued := true
	select {
	case m.manualSlots <- struct{}{}:
	case <-job.ctx.Done():
		queued = false
	}
	if !queued {
		e.mu.Lock()
		wasCurrent := e.manualJob == job
		e.manualWorkers--
		if wasCurrent {
			e.manualJob = nil
			e.state = StateCanceled
			if e.manualRetained {
				e.state = StateQueued
			}
		}
		e.mu.Unlock()
		m.reconcile(e)
		return
	}
	defer func() { <-m.manualSlots }()

	e.mu.Lock()
	if job.canceled {
		e.mu.Unlock()
		m.finishManual(e, job, context.Canceled)
		return
	}
	e.state = StateDownloading
	e.mu.Unlock()

	err := m.downloadAll(job.ctx, e, job.sourceURL)
	m.finishManual(e, job, err)
}

func (m *Manager) finishManual(e *entry, job *manualJob, err error) {
	e.mu.Lock()
	wasCurrent := e.manualJob == job
	e.manualWorkers--
	if wasCurrent {
		e.manualJob = nil
	}
	canceled := errors.Is(err, context.Canceled) || job.ctx.Err() != nil
	if wasCurrent {
		switch {
		case canceled:
			if e.manualRetained {
				e.state = StateQueued
			} else {
				e.state = StateCanceled
			}
		case err != nil:
			e.lastErr = safeError(err)
			e.state = StateError
		case e.isFullLocked():
			e.state = StateSaving
		default:
			e.state = StateIdle
		}
	}
	e.mu.Unlock()
	job.cancel()

	m.maybeSave(e)
	m.reconcile(e)
}

func (m *Manager) maybeSave(e *entry) {
	m.mu.Lock()
	e.mu.Lock()
	if m.closed || m.entries[e.id] != e {
		e.mu.Unlock()
		m.mu.Unlock()
		return
	}
	if !e.isFullLocked() || e.saved || e.saving || e.saveAttempted {
		e.mu.Unlock()
		m.mu.Unlock()
		return
	}
	e.saving = true
	e.saveAttempted = true
	e.state = StateSaving
	song := e.song
	cachePath := e.cachePath
	extension := e.extension
	if fromDisposition := dispositionExtension(e.disposition); fromDisposition != "" {
		extension = fromDisposition
	}
	extension = chooseExtension(e.contentType, extension)
	m.wg.Add(1)
	e.mu.Unlock()
	m.mu.Unlock()

	go func() {
		defer m.wg.Done()
		err := m.lib.SaveDownload(song, cachePath, extension)

		e.mu.Lock()
		e.saving = false
		if err != nil {
			e.lastErr = "save download failed"
			e.state = StateError
		} else {
			e.saved = true
			e.lastErr = ""
			e.state = StateComplete
		}
		e.mu.Unlock()
		m.reconcile(e)
	}()
}

func (m *Manager) reconcile(e *entry) {
	e.mu.Lock()
	idle := e.activeHandlers == 0 && e.manualWorkers == 0 && !e.saving
	hasPlayback := e.playback != nil && e.playback.leases > 0
	remove := e.discardRequested && !e.manualRetained && !hasPlayback && idle &&
		(!e.isFullLocked() || e.saved || e.saveAttempted)
	full := e.isFullLocked()
	saved := e.saved
	saveAttempted := e.saveAttempted
	e.mu.Unlock()

	if full && !saved && !saveAttempted {
		m.maybeSave(e)
		return
	}
	if remove {
		m.removeEntry(e)
	}
}

func (m *Manager) removeEntry(e *entry) {
	m.mu.Lock()
	e.mu.Lock()
	idle := e.activeHandlers == 0 && e.manualWorkers == 0 && !e.saving
	canRemove := !m.closed && idle && e.discardRequested && !e.manualRetained &&
		(e.playback == nil || e.playback.leases == 0)
	if canRemove && m.entries[e.id] == e {
		delete(m.entries, e.id)
	} else {
		canRemove = false
	}
	e.mu.Unlock()
	m.mu.Unlock()

	if canRemove {
		_ = os.Remove(e.cachePath)
	}
}

func (m *Manager) finishHandler(e *entry, run *playbackRun) {
	e.mu.Lock()
	if e.activeHandlers > 0 {
		e.activeHandlers--
	}
	if run.handlers > 0 {
		run.handlers--
	}
	e.mu.Unlock()
	m.reconcile(e)
}

func (m *Manager) recordTransferError(e *entry, err error) {
	if err == nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return
	}
	e.mu.Lock()
	e.lastErr = safeError(err)
	e.state = StateError
	e.mu.Unlock()
}

func (e *entry) isFullLocked() bool {
	return e.total > 0 && coveredBytes(e.ranges) == e.total
}

func (e *entry) statusLocked() Status {
	state := e.state
	switch {
	case e.lastErr != "":
		state = StateError
	case e.saving:
		state = StateSaving
	case e.manualJob != nil && e.state == StateQueued:
		state = StateQueued
	case e.manualJob != nil && e.state == StateDownloading:
		state = StateDownloading
	case e.activeHandlers > 0:
		state = StateStreaming
	case e.saved || e.isFullLocked():
		state = StateComplete
	case state == StateCanceled:
		state = StateCanceled
	case state != StateError:
		state = StateIdle
	}
	total := e.total
	if total < 0 {
		total = 0
	}
	return Status{
		Song:  e.song,
		Bytes: coveredBytes(e.ranges),
		Total: total,
		State: state,
		Error: e.lastErr,
	}
}

func rawURL(raw string) (string, error) {
	u, err := url.Parse(raw)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return "", errors.New("transfer: invalid source URL")
	}
	query := u.Query()
	query.Set("format", "raw")
	u.RawQuery = query.Encode()
	return u.String(), nil
}

func randomToken() (string, error) {
	var data [16]byte
	if _, err := rand.Read(data[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(data[:]), nil
}

func safeError(err error) string {
	switch {
	case err == nil:
		return ""
	case errors.Is(err, context.Canceled):
		return "canceled"
	case errors.Is(err, context.DeadlineExceeded):
		return "timed out"
	default:
		return err.Error()
	}
}
