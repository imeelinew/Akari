package ui

import (
	"akari/internal/core"
	"akari/internal/player"
	tea "charm.land/bubbletea/v2"
	"fmt"
	uv "github.com/charmbracelet/ultraviolet"
	"strings"
	"time"
)

func (m *Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	if _, ok := msg.(tea.RawMsg); ok {
		return m, nil
	}
	var cmds []tea.Cmd
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.artRecovery = 0
		m.width, m.height = msg.Width, msg.Height
		m.resize()
		cmds = append(cmds, m.paintLater())
	case tea.BackgroundColorMsg:
		m.dark = msg.IsDark()
		m.theme()
		m.syncView()
		cmds = append(cmds, m.paintLater())
	case tea.FocusMsg:
		m.artRecovery = 0
		cmds = append(cmds, m.paintLater())
	case albumsMsg:
		m.loading = false
		if len(msg.albums) > 0 {
			m.albums = msg.albums
		}
		m.check(msg.err)
		m.syncList()
	case songsMsg:
		if msg.download {
			if msg.err != nil {
				m.check(msg.err)
			} else {
				for _, s := range msg.songs {
					m.check(m.transfers.Download(s))
				}
				m.notice = "Download queued"
			}
		} else if m.album != nil && m.album.ID == msg.id {
			m.loading = false
			m.songs = msg.songs
			m.check(msg.err)
			m.syncList()
			m.reveal()
		}
	case artMsg:
		if msg.request == m.artRequest && msg.key == m.artKey {
			m.artData = msg.data
			m.artRetryAt = time.Time{}
			if msg.err != nil {
				m.artStatus = "Artwork unavailable"
				m.artAttempts++
				if m.artAttempts < 3 {
					m.artRetryAt = time.Now().Add(time.Duration(m.artAttempts) * time.Second)
				}
			} else {
				m.artStatus = ""
			}
			cmds = append(cmds, m.paintLater())
		}
	case paintMsg:
		if int(msg) != m.paint {
			return m, nil
		}
		return m, tea.Raw(artworkFrame{m, int(msg)})
	case uv.KittyGraphicsEvent:
		if strings.HasPrefix(string(msg.Payload), "ENOENT") {
			id := uint32(msg.Options.ID)
			for key, cached := range m.imageIDs {
				if cached == id {
					delete(m.imageIDs, key)
					m.dropImageOrder(key)
					if key == m.artKey {
						m.artRecovery++
						if m.artRecovery > 2 {
							m.artData = nil
							m.artStatus = "Artwork unavailable"
						}
						cmds = append(cmds, m.paintLater())
					}
					break
				}
			}
		}

	case player.Event:
		cmds = append(cmds, m.playerEvent())
		if msg.Generation != 0 && msg.Generation != m.player.Generation() {
			break
		}
		switch msg.Name {
		case "time-pos":
			if v, ok := msg.Value.(float64); ok {
				m.state.Position = v
			}
		case "duration":
			if v, ok := msg.Value.(float64); ok {
				m.duration = v
			}
		case "pause":
			if v, ok := msg.Value.(bool); ok && (m.loaded || m.buffering) {
				m.paused = v
			}
		case "spatial":
			if enabled, ok := msg.Value.(bool); ok {
				m.spatial = enabled
			}
		case "volume":
			if v, ok := msg.Value.(float64); ok {
				m.state.Volume = v
			}
		case "paused-for-cache":
			if v, ok := msg.Value.(bool); ok {
				m.buffering = v
			}
		case "file-loaded":
			if m.current() == nil {
				break
			}
			m.loaded = true
			m.buffering = false
			m.check(m.player.Volume(m.state.Volume))
		case "end-file":
			if reason, ok := msg.Value.(string); ok {
				if reason == "eof" && m.loaded {
					cmds = append(cmds, m.next(false))
				}
				if reason == "error" {
					m.buffering = false
					m.paused = true
					m.loaded = false
					m.notice = "Playback failed · space retry"
				}
			}
		case "disconnected":
			m.notice = "Player disconnected"
			m.loaded = false
			m.paused = true
			cmds = nil
		case "error":
			m.check(fmt.Errorf("%s", msg.Error))
		}
	case tickMsg:
		if m.page == transfersPage || m.page == downloadedPage {
			m.syncList()
		}
		if time.Since(m.persistAt) > 5*time.Second {
			m.save()
		}
		cmds = append(cmds, tick())
	case actionMsg:
		m.check(msg.err)
		m.syncList()
	case tea.KeyPressMsg:
		if msg.String() == "ctrl+c" {
			return m, tea.Quit
		}
		if m.filtering {
			switch msg.String() {
			case "enter":
				m.filtering = false
				m.search.Blur()
			case "esc":
				m.filtering = false
				m.search.Blur()
				m.search.SetValue("")
				m.syncList()
				m.reveal()
			default:
				var c tea.Cmd
				m.search, c = m.search.Update(msg)
				cmds = append(cmds, c)
				m.syncList()
				m.list.Select(0)
				m.syncView()
				m.view.GotoTop()
			}
			break
		}
		switch msg.String() {
		case "q":
			return m, tea.Quit
		case "?":
			m.showHelp = !m.showHelp
			m.resize()
			cmds = append(cmds, m.paintLater())
		case "/":
			m.filtering = true
			cmds = append(cmds, m.search.Focus())
		case "esc":
			m.confirm = ""
			if m.showHelp {
				m.showHelp = false
				m.resize()
				cmds = append(cmds, m.paintLater())
			} else if m.search.Value() != "" {
				m.search.SetValue("")
				m.syncList()
			} else if m.album != nil {
				id := m.album.ID
				m.album = nil
				m.songs = nil
				m.loading = false
				m.syncList()
				for i, it := range m.list.Items() {
					e := it.(entry)
					if e.album != nil && e.album.ID == id {
						m.list.Select(i)
						break
					}
				}
				m.syncView()
				m.reveal()
			} else {
				m.notice = ""
			}
		case "1":
			m.setPage(albumsPage)
		case "2":
			m.setPage(downloadedPage)
		case "3":
			m.setPage(queuePage)
		case "4":
			m.setPage(transfersPage)
		case "tab", "shift+tab":
			step := 1
			if msg.String() == "shift+tab" {
				step = 3
			}
			m.setPage(page((int(m.page) + step) % 4))
		case "enter":
			cmds = append(cmds, m.activate())
		case "space":
			cmds = append(cmds, m.toggle())
		case "n":
			cmds = append(cmds, m.next(true))
		case "p":
			cmds = append(cmds, m.previous())
		case "left", "h":
			if m.loaded {
				m.check(m.player.Seek(-5, false))
			} else {
				m.state.Position = max(0, m.state.Position-5)
			}
		case "right", "l":
			if m.loaded {
				m.check(m.player.Seek(5, false))
			} else {
				m.state.Position = min(m.duration, m.state.Position+5)
			}
		case "+", "=":
			m.changeVolume(5)
		case "-":
			m.changeVolume(-5)
		case "o":
			m.toggleSpatial()
		case "s":
			m.state.Shuffle = !m.state.Shuffle
		case "r":
			m.state.Repeat = (m.state.Repeat + 1) % 3
		case "R":
			m.loading = true
			cmds = append(cmds, m.loadAlbums(true))
		case "d":
			cmds = append(cmds, m.download())
		case "C":
			if m.activeSongID != "" {
				m.transfers.Release(m.activeSongID)
			}
			m.activeSongID = ""
			m.check(m.player.Stop())
			m.loaded = false
			m.paused = true
			m.buffering = false
			m.state.Queue = nil
			m.state.Index = -1
			m.state.Position = 0
			m.duration = 0
			m.history = nil
			m.syncList()
		case "c":
			if m.page == queuePage {
				if m.state.Index >= 0 {
					m.state.Queue = m.state.Queue[:m.state.Index+1]
				}
				m.syncList()
			} else if s := m.selectedSong(); s != nil {
				m.transfers.Cancel(s.ID)
				m.syncList()
			} else if a := m.selectedAlbum(); a != nil {
				for _, t := range m.transfers.Snapshot() {
					if t.Song.AlbumID == a.ID {
						m.transfers.Cancel(t.Song.ID)
					}
				}
			}
		case "a", "N":
			if s := m.selectedSong(); s != nil {
				if m.page != queuePage {
					if msg.String() == "N" && m.state.Index >= 0 {
						idx := m.state.Index + 1
						m.state.Queue = append(m.state.Queue[:idx], append([]coreSong{*s}, m.state.Queue[idx:]...)...)
					} else {
						m.state.Queue = append(m.state.Queue, *s)
					}
					m.history = nil
					m.notice = "Added to queue"
				}
			}
		case "x":
			cmds = append(cmds, m.remove())
		case "J", "K":
			if m.page == queuePage {
				m.moveQueue(msg.String() == "J")
			}
		case "up", "down", "j", "k", "pgup", "pgdown", "home", "end", "g", "G":
			m.confirm = ""
			switch msg.String() {
			case "home", "g":
				m.list.Select(0)
			case "end", "G":
				m.list.Select(len(m.list.Items()) - 1)
			default:
				var c tea.Cmd
				m.list, c = m.list.Update(msg)
				cmds = append(cmds, c)
			}
			m.syncView()
			m.reveal()
		}
	case tea.MouseWheelMsg:
		if m.volumeAt(msg.X, msg.Y) {
			if msg.Button == tea.MouseWheelUp {
				m.changeVolume(5)
			} else if msg.Button == tea.MouseWheelDown {
				m.changeVolume(-5)
			}
			break
		}
		if msg.Y >= 4 && msg.Y < 4+m.listH && msg.X < m.left {
			if msg.Button == tea.MouseWheelUp {
				m.view.ScrollUp(2)
			} else if msg.Button == tea.MouseWheelDown {
				m.view.ScrollDown(2)
			}
		}
	case tea.MouseClickMsg:
		if msg.Button == tea.MouseLeft {
			cmds = append(cmds, m.click(msg.X, msg.Y))
		}
	}
	cmds = append(cmds, m.artwork())
	return m, tea.Batch(cmds...)
}

func (m *Model) remove() tea.Cmd {
	e, ok := m.list.SelectedItem().(entry)
	if !ok {
		return nil
	}
	if m.page == queuePage && e.song != nil {
		idx := e.index
		current := idx == m.state.Index
		m.state.Queue = append(m.state.Queue[:idx], m.state.Queue[idx+1:]...)
		m.history = nil
		if idx < m.state.Index {
			m.state.Index--
		}
		m.syncList()
		if current {
			m.transfers.Release(e.song.ID)
			if idx < len(m.state.Queue) {
				return m.play(idx, false, 0)
			}
			_ = m.player.Stop()
			m.loaded = false
			m.paused = true
			m.state.Index = -1
			m.state.Position = 0
		}
		return nil
	}
	if m.page == transfersPage && e.song != nil {
		m.transfers.Cancel(e.song.ID)
		m.syncList()
		return nil
	}
	id := entryID(e)
	if m.confirm != id {
		m.confirm = id
		m.notice = "Press x again to remove local download"
		return nil
	}
	m.confirm = ""
	m.notice = ""
	var ids []string
	if e.song != nil {
		ids = append(ids, e.song.ID)
	} else if e.album != nil {
		for _, s := range m.lib.DownloadedSongs(e.album.ID) {
			if m.lib.IsOwned(s.ID) {
				ids = append(ids, s.ID)
			}
		}
	}
	if len(ids) == 0 {
		m.notice = "External files are read-only"
	}
	for _, id := range ids {
		if !m.lib.IsOwned(id) {
			m.notice = "External file is read-only"
			continue
		}
		if s := m.current(); s != nil && s.ID == id && m.loaded {
			m.notice = "Stop playback before removing this track"
			continue
		}
		m.transfers.Cancel(id)
		m.check(m.lib.Remove(id))
	}
	m.syncList()
	return nil
}
func (m *Model) moveQueue(down bool) {
	e, ok := m.list.SelectedItem().(entry)
	if !ok {
		return
	}
	i := e.index
	j := i - 1
	if down {
		j = i + 1
	}
	if j < 0 || j >= len(m.state.Queue) {
		return
	}
	m.state.Queue[i], m.state.Queue[j] = m.state.Queue[j], m.state.Queue[i]
	if m.state.Index == i {
		m.state.Index = j
	} else if m.state.Index == j {
		m.state.Index = i
	}
	m.history = nil
	m.syncList()
	for n, it := range m.list.Items() {
		if it.(entry).index == j {
			m.list.Select(n)
		}
	}
	m.syncView()
	m.reveal()
}
func (m *Model) click(x, y int) tea.Cmd {
	if y == 0 {
		start := 8
		for i, label := range []string{"Albums", "Downloaded", "Queue", "Transfers"} {
			end := start + len(label) + 2
			if x >= start && x < end {
				m.setPage(page(i))
				return nil
			}
			start = end + 2
		}
	}

	if y >= 4 && y < 4+m.listH && x < m.left && x >= 1 {
		idx := (m.view.YOffset() + y - 4) / 2
		if idx >= 0 && idx < len(m.list.Items()) {
			double := idx == m.lastClickIndex && time.Since(m.lastClick) < 400*time.Millisecond
			m.list.Select(idx)
			m.syncView()
			m.lastClick = time.Now()
			m.lastClickIndex = idx
			if double {
				return m.activate()
			}
		}
		return nil
	}
	if y == m.bodyH+2 {
		start := 9
		w := m.bar.Width()
		if x >= start && x < start+w && m.duration > 0 {
			position := float64(x-start) / float64(max(1, w-1)) * m.duration
			m.state.Position = position
			if m.loaded {
				m.check(m.player.Seek(position, true))
			}
		}
	}
	if y == m.bodyH+3 {
		_, regions := m.controlRow()
		for _, r := range regions {
			if x < r.start || x >= r.end || x >= m.width {
				continue
			}
			switch r.action {
			case "play":
				return m.toggle()
			case "prev":
				return m.previous()
			case "next":
				return m.next(true)
			case "shuffle":
				m.state.Shuffle = !m.state.Shuffle
			case "repeat":
				m.state.Repeat = (m.state.Repeat + 1) % 3
			case "spatial":
				m.toggleSpatial()
			case "volume-down":
				m.changeVolume(-5)
			case "volume-up":
				m.changeVolume(5)
			}
		}
	}

	return nil
}

// Keep the queue insertion expression readable without a second model type.
type coreSong = core.Song

func (m *Model) changeVolume(delta float64) {
	m.state.Volume = min(100, max(0, m.state.Volume+delta))
	m.check(m.player.Volume(m.state.Volume))
}

func (m *Model) toggleSpatial() {
	enabled := !m.spatial
	if err := m.player.SetSpatial(enabled); err != nil {
		m.check(err)
		return
	}
	m.spatial = enabled
}
