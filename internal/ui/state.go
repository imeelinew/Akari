package ui

import (
	"akari/internal/core"
	"encoding/json"
	"os"
	"path/filepath"
)

type playbackState struct {
	Queue    []core.Song        `json:"queue"`
	Index    int                `json:"index"`
	Position float64            `json:"position"`
	Volume   float64            `json:"volume"`
	Shuffle  bool               `json:"shuffle"`
	Repeat   int                `json:"repeat"`
	Recent   map[string]float64 `json:"recent"`
}

func readState(dir string) playbackState {
	s := playbackState{Index: -1, Volume: 70, Recent: map[string]float64{}}
	if b, e := os.ReadFile(filepath.Join(dir, "playback.json")); e == nil {
		_ = json.Unmarshal(b, &s)
	}
	if s.Recent == nil {
		s.Recent = map[string]float64{}
	}
	s.Volume = min(100, max(0, s.Volume))
	s.Repeat = max(0, min(2, s.Repeat))
	if s.Index < 0 || s.Index >= len(s.Queue) {
		s.Index = -1
	}
	return s
}
func writeState(dir string, s playbackState) error {
	if err := os.MkdirAll(dir, 0700); err != nil {
		return err
	}
	s.Queue = append([]core.Song(nil), s.Queue...)
	for i := range s.Queue {
		s.Queue[i].ArtworkURL = ""
	}
	b, err := json.Marshal(s)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".playback-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_ = f.Chmod(0600)
	if _, err = f.Write(b); err != nil {
		f.Close()
		return err
	}
	if err = f.Close(); err != nil {
		return err
	}
	return os.Rename(f.Name(), filepath.Join(dir, "playback.json"))
}
