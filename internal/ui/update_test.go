package ui

import (
	"testing"

	"charm.land/bubbles/v2/viewport"
)

func TestRepaintAfterViewportScroll(t *testing.T) {
	m := &Model{view: viewport.New()}
	m.view.SetWidth(10)
	m.view.SetHeight(2)
	m.view.SetContent("one\ntwo\nthree")
	m.view.SetYOffset(1)

	if cmd := m.repaintAfterViewportScroll(1); cmd != nil {
		t.Fatal("unchanged viewport offset scheduled an artwork repaint")
	}
	if cmd := m.repaintAfterViewportScroll(0); cmd == nil {
		t.Fatal("changed viewport offset did not schedule an artwork repaint")
	}
}
