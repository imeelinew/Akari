package ui

import (
	"fmt"
	"math"
	"strings"
	"time"

	"akari/internal/artwork"
	tea "charm.land/bubbletea/v2"
)

type spectrumMsg time.Time

type spectrumCache struct {
	width, height int
	palette       artwork.Palette
	glyphs        [9]string
	colors        []string
}

func (m *Model) spectrumTick() tea.Cmd {
	if m.spectrumPending || !m.focused || m.spectrumHeight() == 0 || m.showHelp || m.selectedAlbum() == nil {
		return nil
	}
	active := !m.paused && !m.buffering
	for _, v := range m.spectrum {
		active = active || v > 0
	}
	if !active {
		return nil
	}
	m.spectrumPending = true
	return tea.Tick(time.Second/30, func(t time.Time) tea.Msg { return spectrumMsg(t) })
}
func (m *Model) spectrumHeight() int {
	if m.width < 76 || m.bodyH < 18 {
		return 0
	}
	return min(8, m.bodyH/4)
}
func (m *Model) spectrumReserve() int {
	if h := m.spectrumHeight(); h > 0 {
		return h + 1
	}
	return 0
}
func (m *Model) spectrumLines(width int) []string {
	height := m.spectrumHeight()
	if height == 0 {
		return nil
	}
	bars := min(32, max(1, width/3))
	barWidth := max(1, (width-(bars-1))/bars)
	used := bars*barWidth + bars - 1
	padding := (width - used) / 2
	cache := &m.spectrumPaint
	if cache.width != width || cache.height != height || cache.palette != m.spectrumColors {
		cache.width, cache.height, cache.palette = width, height, m.spectrumColors
		for i, glyph := range []rune(" ▁▂▃▄▅▆▇█") {
			cache.glyphs[i] = strings.Repeat(string(glyph), barWidth)
		}
		cache.colors = make([]string, height)
		for row := range cache.colors {
			c := m.spectrumColors.At(float64(height-1-row) / float64(max(1, height-1)))
			cache.colors[row] = fmt.Sprintf("\x1b[38;2;%d;%d;%dm", c.R, c.G, c.B)
		}
	}
	var units [32]int
	for b := 0; b < bars; b++ {
		level := 0.0
		for k := b * 32 / bars; k < (b+1)*32/bars; k++ {
			level = max(level, m.spectrum[k])
		}
		units[b] = int(math.Round(level * float64(height) * 8))
	}
	lines := make([]string, height)
	for row := range lines {
		var out strings.Builder
		out.Grow(width*3 + 40)
		out.WriteString(cache.colors[row])
		out.WriteString(strings.Repeat(" ", padding))
		for b := 0; b < bars; b++ {
			out.WriteString(cache.glyphs[max(0, min(8, units[b]-8*(height-1-row)))])
			if b+1 < bars {
				out.WriteByte(' ')
			}
		}
		out.WriteString(strings.Repeat(" ", width-used-padding))
		out.WriteString("\x1b[39m")
		lines[row] = out.String()
	}
	return lines
}
