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
		m.spectrumColors = artwork.Palette{}
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
		return artMsg{request: request, key: key, data: data, colors: artwork.Colors(data), err: err}
	})
}

// paintLater invalidates the image layer; the renderer composes it after text.
func (m *Model) paintLater() tea.Cmd {
	m.paint++
	return nil
}

type imageUpload struct {
	data                    []byte
	id                      uint32
	col, row, width, height int
}

func (u imageUpload) String() string {
	return artwork.Render(u.data, u.id, u.col, u.row, u.width, u.height)
}

func (m *Model) imageLayer() *tea.ImageLayer {
	if !graphicsEnabled(m) || m.artKey == "" || m.width < 76 || m.height < 16 {
		return nil
	}
	col, row, w, h := m.imageBounds()
	if w < 2 || h < 2 {
		return nil
	}
	id, known := m.imageIDs[m.artKey]
	if !known {
		if len(m.imageOrder) >= 32 {
			oldest := m.imageOrder[0]
			delete(m.imageIDs, oldest)
			m.imageOrder = m.imageOrder[1:]
		}
		m.imageNext++
		id = m.imageNext
		m.imageIDs[m.artKey] = id
	}
	m.dropImageOrder(m.artKey)
	m.imageOrder = append(m.imageOrder, m.artKey)
	return &tea.ImageLayer{ID: id, Revision: uint64(m.paint), Upload: imageUpload{m.artData, id, col, row, w, h}, Place: artwork.Place(id, col, row, w, h), Hide: artwork.Delete(id), Forget: artwork.Release(id)}
}

func (m *Model) dropImageOrder(key string) {
	for i, k := range m.imageOrder {
		if k == key {
			m.imageOrder = append(m.imageOrder[:i], m.imageOrder[i+1:]...)
			return
		}
	}
}
