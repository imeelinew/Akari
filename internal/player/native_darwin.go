//go:build darwin && cgo

package player

/*
#cgo CFLAGS: -x objective-c -fobjc-arc -fblocks -mmacosx-version-min=11.0
#cgo LDFLAGS: -framework Foundation -framework AVFoundation -framework CoreMedia
#include <stdlib.h>
#include "native.h"
*/
import "C"

import (
	"errors"
	"math"
	"net/url"
	"path/filepath"
	"runtime"
	"sync"
	"time"
	"unsafe"
)

// AVFoundation delivers parts of its playback lifecycle on the process main
// thread. Keep that thread for Core Foundation; Bubble Tea runs independently.
func init() { runtime.LockOSThread() }
func Run(work func() error) error {
	result := make(chan error, 1)
	go func() { defer C.ak_main_stop(); result <- work() }()
	C.ak_main_run()
	return <-result
}

type Event struct {
	Name       string
	Value      any
	Error      string
	Generation uint64
}
type Player struct {
	mu         sync.Mutex
	native     unsafe.Pointer
	generation uint64
	spatial    bool
	events     chan Event
	done       chan struct{}
	wg         sync.WaitGroup
	once       sync.Once
}

func New(spatial bool) (*Player, error) {
	native := C.ak_player_create(flag(spatial))
	if native == nil {
		return nil, errors.New("cannot initialize native audio")
	}
	p := &Player{native: native, spatial: spatial, events: make(chan Event, 128), done: make(chan struct{})}
	p.wg.Add(1)
	go p.poll()
	return p, nil
}
func flag(b bool) C.int {
	if b {
		return 1
	}
	return 0
}
func (p *Player) Events() <-chan Event { return p.events }
func (p *Player) Generation() uint64   { p.mu.Lock(); defer p.mu.Unlock(); return p.generation }
func (p *Player) Spatial() bool        { p.mu.Lock(); defer p.mu.Unlock(); return p.spatial }
func (p *Player) Load(source string, paused bool, position float64) error {
	if !filepath.IsAbs(source) {
		u, err := url.Parse(source)
		if err != nil || u.Host == "" || (u.Scheme != "http" && u.Scheme != "https") {
			return errors.New("invalid audio source")
		}
	}
	if math.IsNaN(position) || math.IsInf(position, 0) {
		position = 0
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.native == nil {
		return errors.New("player closed")
	}
	p.generation++
	s := C.CString(source)
	defer C.free(unsafe.Pointer(s))
	if C.ak_player_load(p.native, s, flag(paused), C.double(max(0, position)), C.uint64_t(p.generation)) == 0 {
		return errors.New("cannot open audio source")
	}
	return nil
}
func (p *Player) Pause(paused bool) error {
	return p.withNative(func(n unsafe.Pointer) { C.ak_player_pause(n, flag(paused)) })
}
func (p *Player) Seek(seconds float64, absolute bool) error {
	if math.IsNaN(seconds) || math.IsInf(seconds, 0) {
		return errors.New("invalid seek position")
	}
	return p.withNative(func(n unsafe.Pointer) { C.ak_player_seek(n, C.double(seconds), flag(absolute)) })
}
func (p *Player) Volume(volume float64) error {
	if math.IsNaN(volume) || math.IsInf(volume, 0) {
		return errors.New("invalid volume")
	}
	return p.withNative(func(n unsafe.Pointer) { C.ak_player_volume(n, C.double(min(100, max(0, volume)))) })
}
func (p *Player) SetSpatial(enabled bool) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.native == nil {
		return errors.New("player closed")
	}
	C.ak_player_spatial(p.native, flag(enabled))
	p.spatial = enabled
	return nil
}
func (p *Player) Stop() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.native == nil {
		return errors.New("player closed")
	}
	p.generation++
	C.ak_player_stop(p.native, C.uint64_t(p.generation))
	return nil
}
func (p *Player) withNative(fn func(unsafe.Pointer)) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.native == nil {
		return errors.New("player closed")
	}
	fn(p.native)
	return nil
}
func (p *Player) Close() error {
	p.once.Do(func() {
		close(p.done)
		p.wg.Wait()
		p.mu.Lock()
		defer p.mu.Unlock()
		if p.native != nil {
			C.ak_player_destroy(p.native)
			p.native = nil
		}
	})
	return nil
}
func (p *Player) poll() {
	defer p.wg.Done()
	defer close(p.events)
	ticker := time.NewTicker(200 * time.Millisecond)
	defer ticker.Stop()
	var generation uint64
	ready, failed, ended := false, false, false
	var paused, buffering bool
	var duration float64
	lastVolume := math.NaN()
	var lastSpatial bool
	spatialKnown := false
	emit := func(name string, value any, err string) bool {
		select {
		case p.events <- Event{name, value, err, generation}:
			return true
		case <-p.done:
			return false
		}
	}
	for {
		select {
		case <-p.done:
			return
		case <-ticker.C:
		}
		p.mu.Lock()
		if p.native == nil {
			p.mu.Unlock()
			return
		}
		state := C.ak_player_snapshot(p.native)
		p.mu.Unlock()
		if uint64(state.generation) != generation {
			generation = uint64(state.generation)
			ready = false
			failed = false
			ended = false
			duration = 0
			lastVolume = math.NaN()
			spatialKnown = false
			paused = state.paused != 0
			buffering = false
			if generation != 0 && !emit("start-file", nil, "") {
				return
			}
		}
		if state.status == 0 {
			continue
		}
		if state.ended == 0 {
			ended = false
		}
		if ended {
			continue
		}

		if state.status == 3 {
			if !failed {
				failed = true
				if !emit("end-file", "error", "Audio unavailable or unsupported") {
					return
				}
			}
			continue
		}
		if state.status == 2 && !ready {
			ready = true
			if !emit("file-loaded", nil, "") {
				return
			}
			if !emit("pause", state.paused != 0, "") {
				return
			}
		}
		if value := state.paused != 0; value != paused {
			paused = value
			if !emit("pause", value, "") {
				return
			}
		}
		if value := state.buffering != 0; value != buffering {
			buffering = value
			if !emit("paused-for-cache", value, "") {
				return
			}
		}
		if state.status == 2 {
			if v := float64(state.volume); v != lastVolume {
				lastVolume = v
				if !emit("volume", v, "") {
					return
				}
			}
			if v := state.spatial != 0; !spatialKnown || v != lastSpatial {
				lastSpatial = v
				spatialKnown = true
				if !emit("spatial", v, "") {
					return
				}
			}
			if d := float64(state.duration); d > 0 && d != duration {
				duration = d
				if !emit("duration", d, "") {
					return
				}
			}
			if !emit("time-pos", float64(state.position), "") {
				return
			}
			if state.ended != 0 && !ended {
				ended = true
				if !emit("end-file", "eof", "") {
					return
				}
			}
		}
	}
}
