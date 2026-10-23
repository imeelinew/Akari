package ui

import (
	"fmt"
	"math"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

type spectrumMsg time.Time

func (m *Model) spectrumTick() tea.Cmd {
	interval := 250 * time.Millisecond
	active := !m.paused && !m.buffering && m.spectrumHeight() > 0
	for _, v := range m.spectrum {
		active = active || v > 0
	}
	if active {
		interval = time.Second / 30
	}
	return tea.Tick(interval, func(t time.Time) tea.Msg { return spectrumMsg(t) })
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
func (m *Model) spectrumView(width int) string {
	height := m.spectrumHeight()
	if height == 0 {
		return ""
	}
	bars := min(32, max(1, width/3))
	barWidth := max(1, (width-(bars-1))/bars)
	padding := (width - (bars*barWidth + bars - 1)) / 2
	var out strings.Builder
	glyphs := []rune(" ▁▂▃▄▅▆▇█")
	for row := 0; row < height; row++ {
		t := float64(height-1-row) / float64(max(1, height-1))
		c := m.spectrumColors.At(t)
		color := fmt.Sprintf("#%02x%02x%02x", c.R, c.G, c.B)
		style := lipgloss.NewStyle().Foreground(lipgloss.Color(color))
		out.WriteString(strings.Repeat(" ", padding))
		for b := 0; b < bars; b++ {
			level := 0.0
			for k := b * 32 / bars; k < (b+1)*32/bars; k++ {
				level = max(level, m.spectrum[k])
			}
			units := int(math.Round(level*float64(height)*8)) - 8*(height-1-row)
			out.WriteString(style.Render(strings.Repeat(string(glyphs[max(0, min(8, units))]), barWidth)))
			if b+1 < bars {
				out.WriteByte(' ')
			}
		}
		if row+1 < height {
			out.WriteByte('\n')
		}
	}
	return out.String()
}
