package ui

import (
	"context"
	"time"

	"akari/internal/artwork"
	tea "charm.land/bubbletea/v2"
)

func (m *Model) artwork() tea.Cmd {
	a := m.selectedAlbum()
	key, source, remote := "", "", ""
	if a != nil {
		key = a.ID
		source = m.lib.ArtworkPath(a.ID)
		remote = a.ArtworkURL
		if remote == "" {
			for _, album := range m.albums {
				if album.ID == key {
					remote = album.ArtworkURL
					break
				}
			}
		}
		if source == "" {
			source = remote
		}
	}
	changed := key != m.artKey
	if !changed && (m.artRetryAt.IsZero() || time.Now().Before(m.artRetryAt)) {
		return nil
	}
	if m.artCancel != nil {
		m.artCancel()
		m.artCancel = nil
	}
	m.artRequest++
	request := m.artRequest
	m.artKey = key
	m.artRetryAt = time.Time{}
	if changed {
		m.artData = nil
		m.artAttempts = 0
		m.artRecovery = 0
	}
	m.artStatus = "Loading artwork…"
	repaint := m.paintLater()
	if key == "" {
		m.artStatus = ""
		return repaint
	}
	ctx, cancel := context.WithCancel(m.ctx)
	m.artCancel = cancel
	return tea.Batch(repaint, func() tea.Msg {
		data, err := m.art.Load(ctx, key, source)
		if err != nil && ctx.Err() == nil && remote != "" && remote != source {
			data, err = m.art.Load(ctx, key, remote)
		}
		if err == nil {
			_ = m.lib.SaveArtwork(key, data)
		}
		return artMsg{request: request, key: key, data: data, err: err}
	})
}

func (m *Model) paintLater() tea.Cmd {
	m.paint++
	generation := m.paint
	return tea.Tick(30*time.Millisecond, func(time.Time) tea.Msg { return paintMsg(generation) })
}

// Bubble Tea formats Raw payloads on its event loop, just before writing them.
// Delay both sequence construction and terminal-cache bookkeeping until then:
// a queued paint for an older selection must not hide a newer image or claim
// an upload that was never written.
type artworkFrame struct {
	model      *Model
	generation int
}

func (f artworkFrame) String() string {
	if f.generation != f.model.paint {
		return ""
	}
	return f.model.renderArtwork()
}

func (m *Model) renderArtwork() string {
	visible := graphicsEnabled(m) && m.artKey != "" && m.width >= 76 && m.height >= 16
	if !visible {
		seq := artwork.Delete(m.artID)
		m.artID = 0
		return seq
	}
	col, row, w, h := m.imageBounds()
	if w < 2 || h < 2 {
		return ""
	}
	id, uploaded := m.imageIDs[m.artKey]
	seq := ""
	if m.artID != 0 && m.artID != id {
		seq += artwork.Delete(m.artID)
	}
	if !uploaded {
		if len(m.imageOrder) >= 32 {
			oldest := m.imageOrder[0]
			seq += artwork.Forget(m.imageIDs[oldest])
			delete(m.imageIDs, oldest)
			m.dropImageOrder(oldest)
		}
		m.imageNext++
		id = m.imageNext
		upload := artwork.Render(m.artData, id, col, row, w, h)
		if upload == "" {
			return seq
		}
		seq += upload
		m.imageIDs[m.artKey] = id
	} else {
		seq += artwork.Place(id, col, row, w, h)
	}
	m.dropImageOrder(m.artKey)
	m.imageOrder = append(m.imageOrder, m.artKey)
	m.artID = id
	return seq
}

func (m *Model) dropImageOrder(key string) {
	for i, k := range m.imageOrder {
		if k == key {
			m.imageOrder = append(m.imageOrder[:i], m.imageOrder[i+1:]...)
			return
		}
	}
}
