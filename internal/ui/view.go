package ui

import (
	"akari/internal/artwork"
	"bytes"
	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"fmt"
	"github.com/charmbracelet/x/ansi"
	"golang.org/x/sys/unix"
	"image/png"
	"os"
	"strings"
)

type bindings struct{}

func bind(label, desc string) key.Binding {
	return key.NewBinding(key.WithKeys(label), key.WithHelp(label, desc))
}
func (bindings) ShortHelp() []key.Binding {
	return []key.Binding{bind("enter", "open / play"), bind("space", "pause"), bind("/", "search"), bind("?", "help"), bind("q", "quit")}
}
func (bindings) FullHelp() [][]key.Binding {
	return [][]key.Binding{
		{bind("↑↓ / j k", "move"), bind("enter", "open / play"), bind("esc", "back / clear"), bind("/", "search"), bind("tab / 1–4", "view")},
		{bind("space", "play / pause"), bind("p / n", "previous / next"), bind("← →", "seek 5s"), bind("+ −", "volume"), bind("s / r", "shuffle / repeat"), bind("o", "spatial audio")},
		{bind("a / N", "queue / play next"), bind("J / K", "reorder queue"), bind("c / C", "cancel / stop & clear"), bind("d / x", "download / remove"), bind("R", "refresh")},
		{bind("click", "select"), bind("double-click", "open / play"), bind("wheel", "scroll"), bind("?", "close help"), bind("q", "quit")},
	}
}
func (m *Model) muted() lipgloss.Style {
	color := "240"
	if !m.dark {
		color = "244"
	}
	return lipgloss.NewStyle().Foreground(lipgloss.Color(color))
}
func (m *Model) imageBounds() (int, int, int, int) {
	w, h := max(1, m.width-m.left-4), max(1, m.bodyH-8)
	cw, ch := 1.0, 2.0
	if ws, err := unix.IoctlGetWinsize(int(os.Stdout.Fd()), unix.TIOCGWINSZ); err == nil && ws.Xpixel > 0 && ws.Ypixel > 0 && ws.Col > 0 && ws.Row > 0 {
		cw = float64(ws.Xpixel) / float64(ws.Col)
		ch = float64(ws.Ypixel) / float64(ws.Row)
	}
	if cfg, err := png.DecodeConfig(bytes.NewReader(m.artData)); err == nil {
		ratio := float64(cfg.Width) / float64(cfg.Height) * ch / cw
		cols := min(w, max(1, int(float64(h)*ratio)))
		rows := min(h, max(1, int(float64(cols)/ratio)))
		return m.left + 3 + (w-cols)/2, 5 + (h-rows)/2, cols, rows
	}
	return m.left + 3, 5, w, h
}

func (m *Model) View() tea.View {
	v := tea.NewView("")
	v.AltScreen = true
	v.ReportFocus = true
	v.MouseMode = tea.MouseModeCellMotion
	v.WindowTitle = "Akari"
	if m.width == 0 || m.height == 0 {
		return v
	}
	if m.width < 40 || m.height < 16 {
		v.SetContent(ansi.Truncate("Akari · resize to 40 × 16 · q quit", m.width, ""))
		return v
	}
	muted := m.muted()
	accent := lipgloss.NewStyle().Foreground(lipgloss.Color("62")).Bold(true)
	tabs := []string{"Albums", "Downloaded", "Queue", "Transfers"}
	for i := range tabs {
		tabs[i] = " " + tabs[i] + " "
		if page(i) == m.page {
			tabs[i] = lipgloss.NewStyle().Reverse(true).Render(tabs[i])
		}
	}
	top := " Akari  " + strings.Join(tabs, "  ")
	title := []string{"Albums", "Downloaded", "Queue", "Transfers"}[m.page]
	if m.album != nil && (m.page == albumsPage || m.page == downloadedPage) {
		title = m.album.Title
	}
	header := ansi.Truncate(clean(title), max(1, m.left-12), "…")
	header += muted.Render(fmt.Sprintf("  %d", len(m.list.Items())))
	search := ""
	if m.filtering || m.search.Value() != "" {
		search = m.search.View()
	} else {
		search = muted.Render("/ Search")
	}
	content := header + "\n" + search + "\n"
	if len(m.list.Items()) == 0 {
		empty := "No albums"
		if m.loading {
			empty = "Loading…"
		} else if m.search.Value() != "" {
			empty = "No matches"
		} else if m.page == queuePage {
			empty = "Queue is empty"
		} else if m.page == transfersPage {
			empty = "No transfers"
		} else if m.album != nil {
			empty = "No tracks"
		}
		content += lipgloss.NewStyle().Height(m.listH).Render(muted.Render(empty))
	} else {
		content += m.view.View()
	}
	panel := lipgloss.NewStyle().Border(lipgloss.NormalBorder()).BorderForeground(lipgloss.Color("240")).Padding(0, 1)
	left := panel.Width(max(1, m.left)).Height(max(1, m.bodyH)).Render(content)
	body := left
	if m.width >= 76 {
		w := m.width - m.left - 4
		h := m.bodyH - 2
		artBody := ""
		if a := m.selectedAlbum(); a != nil {
			artBody = accent.Render(ansi.Truncate(clean(a.Title), w, "…")) + "\n" + muted.Render(ansi.Truncate(clean(a.Artist), w, "…"))
			lines := max(1, h-5)
			placeholder := ""
			if !graphicsEnabled(m) {
				placeholder = m.artStatus
				if placeholder == "" {
					placeholder = "No artwork"
				}
			}
			artBody += "\n" + lipgloss.Place(w, lines, lipgloss.Center, lipgloss.Center, muted.Render(placeholder))
			detail := a.Genre
			if a.Year > 0 {
				if detail != "" {
					detail += " · "
				}
				detail += fmt.Sprint(a.Year)
			}
			artBody += "\n" + muted.Render(ansi.Truncate(clean(detail), w, "…"))
		} else {
			artBody = lipgloss.Place(w, h, lipgloss.Center, lipgloss.Center, muted.Render("Select an album"))
		}
		right := panel.Width(max(1, m.width-m.left)).Height(max(1, m.bodyH)).Render(artBody)
		body = lipgloss.JoinHorizontal(lipgloss.Top, left, right)
	}
	track := "Nothing playing"
	if s := m.current(); s != nil {
		track = clean(s.Title) + " · " + clean(s.Artist)
	}
	player := ansi.Truncate(" "+track, m.width, "…")
	fraction := 0.0
	if m.duration > 0 {
		fraction = min(1, max(0, m.state.Position/m.duration))
	}
	bar := m.bar.ViewAs(fraction)
	progress := " " + fmt.Sprintf("%6s", duration(m.state.Position)) + "  " + bar + " " + duration(m.duration)
	controls, _ := m.controlRow()
	notice := m.notice
	if m.confirm != "" {
		notice = "Press x again to remove local download"
	}
	footer := m.help.View(bindings{})
	result := strings.Join([]string{ansi.Truncate(top, m.width, ""), body, player, ansi.Truncate(progress, m.width, ""), ansi.Truncate(controls, m.width, ""), muted.Render(ansi.Truncate(" "+notice, m.width, "…")), footer}, "\n")
	v.SetContent(lipgloss.NewStyle().MaxWidth(m.width).MaxHeight(m.height).Render(result))
	return v
}

func graphicsEnabled(m *Model) bool { return artwork.Supported() && len(m.artData) > 0 && !m.showHelp }

type control struct {
	start, end int
	action     string
}

func (m *Model) controlRow() (string, []control) {
	mode := "Play"
	if !m.paused {
		mode = "Pause"
	}
	if m.buffering {
		mode = "Wait"
	}
	shuffle := "Shuffle off"
	if m.state.Shuffle {
		shuffle = "Shuffle on"
	}
	repeat := []string{"Repeat off", "Repeat all", "Repeat one"}[m.state.Repeat]
	var b strings.Builder
	b.WriteString(" ")
	var regions []control
	button := func(label, action, gap string) {
		start := b.Len()
		b.WriteString("[" + label + "]")
		regions = append(regions, control{start, b.Len(), action})
		b.WriteString(gap)
	}
	button(mode, "play", " ")
	button("Prev", "prev", " ")
	button("Next", "next", " ")
	button(shuffle, "shuffle", " ")
	button(repeat, "repeat", " ")
	spatial := "Spatial off"
	if m.spatial {
		spatial = "Spatial auto"
	}
	button(spatial, "spatial", " ")
	button("-", "volume-down", " ")
	fmt.Fprintf(&b, "Vol %d%% ", int(m.state.Volume))
	button("+", "volume-up", "")
	return b.String(), regions
}
func (m *Model) volumeAt(x, y int) bool {
	if y != m.bodyH+3 {
		return false
	}
	_, regions := m.controlRow()
	start, end := -1, -1
	for _, r := range regions {
		if r.action == "volume-down" {
			start = r.start
		}
		if r.action == "volume-up" {
			end = r.end
		}
	}
	return x >= start && x < end
}
