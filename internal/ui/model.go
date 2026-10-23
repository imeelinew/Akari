package ui

import (
	"context"
	"fmt"
	"math/rand/v2"
	"sort"
	"strings"
	"time"

	"akari/internal/artwork"
	"akari/internal/core"
	"akari/internal/library"
	"akari/internal/player"
	"akari/internal/transfer"
	"charm.land/bubbles/v2/help"
	"charm.land/bubbles/v2/list"
	"charm.land/bubbles/v2/progress"
	"charm.land/bubbles/v2/textinput"
	"charm.land/bubbles/v2/viewport"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

type page int

const (
	albumsPage page = iota
	downloadedPage
	queuePage
	transfersPage
)

type entry struct {
	album  *core.Album
	song   *core.Song
	status string
	index  int
}

func (e entry) Title() string {
	if e.album != nil {
		return clean(e.album.Title)
	}
	if e.song != nil {
		prefix := ""
		if e.song.TrackNumber > 0 {
			prefix = fmt.Sprintf("%02d  ", e.song.TrackNumber)
		}
		return prefix + clean(e.song.Title)
	}
	return ""
}
func (e entry) Description() string {
	if e.album != nil {
		return clean(e.album.Artist)
	}
	if e.song != nil {
		return clean(e.song.Artist) + " · " + duration(e.song.Duration) + e.status
	}
	return ""
}
func (e entry) FilterValue() string { return e.Title() + " " + e.Description() }

type albumsMsg struct {
	albums []core.Album
	err    error
}
type songsMsg struct {
	id       string
	songs    []core.Song
	err      error
	download bool
}
type artMsg struct {
	request int
	key     string
	data    []byte
	err     error
}
type tickMsg time.Time
type paintMsg int
type actionMsg struct{ err error }

type Model struct {
	spatial                           bool
	artRequest                        int
	artAttempts                       int
	artRecovery                       int
	artRetryAt                        time.Time
	imageOrder                        []string
	artCancel                         context.CancelFunc
	activeSongID                      string
	ctx                               context.Context
	cancel                            context.CancelFunc
	lib                               *library.Library
	player                            *player.Player
	transfers                         *transfer.Manager
	art                               *artwork.Cache
	list                              list.Model
	view                              viewport.Model
	search                            textinput.Model
	help                              help.Model
	bar                               progress.Model
	width, height, left, bodyH, listH int
	dark                              bool
	page                              page
	album                             *core.Album
	albums                            []core.Album
	songs                             []core.Song
	filtering, showHelp, loading      bool
	notice                            string
	focus                             int
	state                             playbackState
	paused, buffering, loaded         bool
	duration                          float64
	history                           []int
	artKey                            string
	artData                           []byte
	artID                             uint32
	imageIDs                          map[string]uint32
	imageNext                         uint32
	paint                             int
	artStatus                         string
	lastClick                         time.Time
	lastClickIndex                    int
	persistAt                         time.Time
	confirm                           string
}

func New(lib *library.Library, p *player.Player, t *transfer.Manager) *Model {
	ctx, cancel := context.WithCancel(context.Background())
	d := list.NewDefaultDelegate()
	d.SetSpacing(0)
	l := list.New(nil, d, 40, 20)
	l.SetShowTitle(false)
	l.SetShowStatusBar(false)
	l.SetShowPagination(false)
	l.SetShowHelp(false)
	l.SetFilteringEnabled(false)
	l.DisableQuitKeybindings()
	v := viewport.New()
	v.FillHeight = true
	v.SoftWrap = false
	input := textinput.New()
	input.Prompt = "/ "
	input.Placeholder = "Search"
	input.SetVirtualCursor(true)
	m := &Model{ctx: ctx, cancel: cancel, lib: lib, player: p, transfers: t, art: artwork.New(lib.CacheDir() + "/artwork"), list: l, view: v, search: input, help: help.New(), bar: progress.New(progress.WithoutPercentage()), dark: true, loading: true, paused: true, state: readState(lib.CacheDir()), imageIDs: map[string]uint32{}, imageNext: 410000, lastClickIndex: -1}
	if song := m.current(); song != nil {
		m.duration = song.Duration
	}
	m.spatial = p.Spatial()
	m.check(p.Volume(m.state.Volume))
	m.theme()
	return m
}
func (m *Model) Init() tea.Cmd {
	return tea.Batch(m.loadAlbums(false), tea.RequestBackgroundColor, tick(), m.playerEvent())
}
func tick() tea.Cmd {
	return tea.Tick(250*time.Millisecond, func(t time.Time) tea.Msg { return tickMsg(t) })
}
func (m *Model) playerEvent() tea.Cmd {
	return func() tea.Msg {
		e, ok := <-m.player.Events()
		if !ok {
			return player.Event{Name: "disconnected"}
		}
		return e
	}
}
func (m *Model) loadAlbums(refresh bool) tea.Cmd {
	return func() tea.Msg { a, e := m.lib.Albums(m.ctx, refresh); return albumsMsg{a, e} }
}
func (m *Model) loadSongs(a core.Album, download bool) tea.Cmd {
	return func() tea.Msg { s, e := m.lib.Songs(m.ctx, a.ID); return songsMsg{a.ID, s, e, download} }
}
func (m *Model) theme() {
	d := list.NewDefaultDelegate()
	d.SetSpacing(0)
	d.Styles = list.NewDefaultItemStyles(m.dark)
	m.list.SetDelegate(d)
	m.list.Styles = list.DefaultStyles(m.dark)
	m.help.Styles = help.DefaultStyles(m.dark)
}
func (m *Model) resize() {
	m.left = m.width
	if m.width >= 76 {
		m.left = m.width * 55 / 100
	}
	m.help.ShowAll = m.showHelp
	m.bodyH = max(3, m.height-6-lipgloss.Height(m.help.View(bindings{})))
	m.listH = max(1, m.bodyH-4)
	m.list.SetWidth(max(1, m.left-4))
	m.view.SetWidth(max(1, m.left-4))
	m.view.SetHeight(m.listH)
	m.search.SetWidth(max(1, m.left-8))
	m.help.SetWidth(max(1, m.width-2))
	m.bar.SetWidth(max(1, m.width-20))
	m.syncList()
	m.reveal()
}
func (m *Model) rows() []entry {
	var out []entry
	if m.page == queuePage {
		for i, s := range m.state.Queue {
			status := ""
			if i == m.state.Index {
				status = " · Current"
			}
			s := s
			out = append(out, entry{song: &s, index: i, status: status})
		}
		return out
	}
	if m.page == transfersPage {
		for _, s := range m.transfers.Snapshot() {
			song := s.Song
			status := " · " + s.State
			if s.Total > 0 {
				status += fmt.Sprintf(" %d%%", 100*s.Bytes/s.Total)
			}
			if s.Error != "" {
				status += " · " + s.Error
			}
			out = append(out, entry{song: &song, status: status})
		}
		return out
	}
	if m.album != nil {
		songs := m.songs
		if m.page == downloadedPage {
			songs = m.lib.DownloadedSongs(m.album.ID)
		}
		for _, s := range songs {
			s := s
			status := ""
			if m.lib.LocalPath(s.ID) != "" {
				status = " · Local"
			}
			out = append(out, entry{song: &s, status: status})
		}
		return out
	}
	albums := append([]core.Album(nil), m.albums...)
	if m.page == downloadedPage {
		albums = m.lib.DownloadedAlbums()
	}
	sort.SliceStable(albums, func(i, j int) bool {
		a, b := max(albums[i].LastPlayed, m.state.Recent[albums[i].ID]), max(albums[j].LastPlayed, m.state.Recent[albums[j].ID])
		if a != b {
			return a > b
		}
		return strings.ToLower(albums[i].Title) < strings.ToLower(albums[j].Title)
	})
	for _, a := range albums {
		a := a
		out = append(out, entry{album: &a})
	}
	return out
}
func (m *Model) syncList() {
	selected := ""
	if e, ok := m.list.SelectedItem().(entry); ok {
		selected = entryID(e)
	}
	query := strings.ToLower(strings.TrimSpace(m.search.Value()))
	var items []list.Item
	for _, e := range m.rows() {
		if query == "" || strings.Contains(strings.ToLower(e.FilterValue()), query) {
			items = append(items, e)
		}
	}
	m.list.SetItems(items)
	m.list.SetHeight(max(1, len(items)*2))
	for i, it := range items {
		if entryID(it.(entry)) == selected {
			m.list.Select(i)
			break
		}
	}
	m.syncView()
}
func entryID(e entry) string {
	if e.album != nil {
		return "a:" + e.album.ID
	}
	if e.song != nil {
		return fmt.Sprintf("s:%s:%d", e.song.ID, e.index)
	}
	return ""
}

func (m *Model) syncView() {
	if len(m.list.Items()) == 0 {
		m.view.SetContent("")
	} else {
		m.view.SetContent(m.list.View())
	}
}

func (m *Model) reveal() {
	line := m.list.Index() * 2
	margin := min(2, max(0, (m.listH-2)/2))
	if line < m.view.YOffset()+margin {
		m.view.SetYOffset(line - margin)
	} else if line+2 > m.view.YOffset()+m.listH-margin {
		m.view.SetYOffset(line + 2 - m.listH + margin)
	}
}

func (m *Model) selectedSong() *core.Song {
	e, ok := m.list.SelectedItem().(entry)
	if ok {
		return e.song
	}
	return nil
}
func (m *Model) selectedAlbum() *core.Album {
	if m.album != nil {
		return m.album
	}
	e, ok := m.list.SelectedItem().(entry)
	if ok && e.album != nil {
		return e.album
	}
	if ok && e.song != nil {
		return &core.Album{ID: e.song.AlbumID, Title: e.song.AlbumTitle, Artist: e.song.Artist, ArtworkURL: e.song.ArtworkURL}
	}
	return nil
}
func (m *Model) current() *core.Song {
	if m.state.Index >= 0 && m.state.Index < len(m.state.Queue) {
		return &m.state.Queue[m.state.Index]
	}
	return nil
}
func (m *Model) play(index int, paused bool, position float64) tea.Cmd {
	if index < 0 || index >= len(m.state.Queue) {
		return nil
	}
	if m.activeSongID != "" {
		m.transfers.Release(m.activeSongID)
	}
	m.activeSongID = ""
	m.state.Index = index
	m.state.Position = position
	m.duration = m.state.Queue[index].Duration
	m.paused = paused
	m.loaded = false
	m.buffering = true
	song := m.state.Queue[index]
	if !paused {
		m.state.Recent[song.AlbumID] = float64(time.Now().Unix())
	}
	m.syncList()
	source, err := m.transfers.Source(song)
	if err == nil {
		m.activeSongID = song.ID
		err = m.player.Load(source, paused, position)
	}
	if err != nil {
		_ = m.player.Stop()
		m.buffering = false
		m.paused = true
		m.check(err)
	}
	return nil
}
func (m *Model) next(manual bool) tea.Cmd {
	if len(m.state.Queue) == 0 {
		return nil
	}
	if !manual && m.state.Repeat == 2 {
		return m.play(m.state.Index, false, 0)
	}
	if m.state.Shuffle && len(m.state.Queue) > 1 {
		m.history = append(m.history, m.state.Index)
		n := rand.IntN(len(m.state.Queue) - 1)
		if n >= m.state.Index {
			n++
		}
		return m.play(n, false, 0)
	}
	n := m.state.Index + 1
	if n >= len(m.state.Queue) {
		if m.state.Repeat == 1 {
			n = 0
		} else {
			if manual {
				return nil
			}
			m.state.Position = 0
			m.paused = true
			m.loaded = false
			m.buffering = false
			if s := m.current(); s != nil {
				m.transfers.Release(s.ID)
			}
			return nil
		}
	}
	return m.play(n, false, 0)
}
func (m *Model) previous() tea.Cmd {
	if m.loaded && m.state.Position > 3 {
		m.check(m.player.Seek(0, true))
		return nil
	}
	if len(m.history) > 0 {
		idx := m.history[len(m.history)-1]
		m.history = m.history[:len(m.history)-1]
		return m.play(idx, false, 0)
	}
	return m.play(max(0, m.state.Index-1), false, 0)
}

func (m *Model) toggle() tea.Cmd {
	if m.current() == nil {
		return m.activate()
	}
	if !m.loaded {
		return m.play(m.state.Index, false, m.state.Position)
	}
	m.paused = !m.paused
	m.check(m.player.Pause(m.paused))
	return nil
}
func (m *Model) activate() tea.Cmd {
	e, ok := m.list.SelectedItem().(entry)
	if !ok {
		return nil
	}
	if e.album != nil {
		m.album = e.album
		m.songs = nil
		m.loading = true
		m.search.SetValue("")
		m.view.GotoTop()
		m.syncList()
		return m.loadSongs(*e.album, false)
	}
	if e.song != nil {
		if m.page == queuePage {
			return m.play(e.index, false, 0)
		}
		if m.page == transfersPage {
			return nil
		}
		var queue []core.Song
		index := 0
		for _, it := range m.list.Items() {
			r := it.(entry)
			if r.song != nil {
				if r.song.ID == e.song.ID {
					index = len(queue)
				}
				queue = append(queue, *r.song)
			}
		}
		m.state.Queue = queue
		m.history = nil
		return m.play(index, false, 0)
	}
	return nil
}
func (m *Model) check(err error) {
	if err != nil {
		m.notice = clean(err.Error())
	}
}
func (m *Model) save() { m.check(writeState(m.lib.CacheDir(), m.state)); m.persistAt = time.Now() }
func (m *Model) Close() {
	m.cancel()
	m.save()
	_ = m.lib.Close()
	_ = m.player.Close()
	_ = m.transfers.Close()
}
func (m *Model) setPage(p page) {
	m.page = p
	m.album = nil
	m.songs = nil
	m.search.SetValue("")
	m.filtering = false
	m.search.Blur()
	m.confirm = ""
	m.view.GotoTop()
	m.syncList()
	m.list.Select(0)
	m.syncView()
}
func (m *Model) download() tea.Cmd {
	if s := m.selectedSong(); s != nil {
		if m.lib.LocalPath(s.ID) != "" {
			m.notice = "Already local"
			return nil
		}
		if err := m.transfers.Download(*s); err != nil {
			m.check(err)
		} else {
			m.notice = "Download queued"
		}
		return nil
	}
	if a := m.selectedAlbum(); a != nil {
		return m.loadSongs(*a, true)
	}
	return nil
}

func clean(s string) string {
	return strings.Map(func(r rune) rune {
		if r < 32 || r == 127 || r >= 0x80 && r <= 0x9f {
			return ' '
		}
		return r
	}, s)
}
func duration(s float64) string { n := max(0, int(s)); return fmt.Sprintf("%d:%02d", n/60, n%60) }
