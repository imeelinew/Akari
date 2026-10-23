package player

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"time"
)

type Event struct {
	Name  string
	Value any
	Error string
}
type reply struct {
	Event  string `json:"event"`
	Name   string `json:"name"`
	Data   any    `json:"data"`
	Error  string `json:"error"`
	Reason string `json:"reason"`
}
type Player struct {
	mu      sync.Mutex
	conn    net.Conn
	cmd     *exec.Cmd
	dir     string
	events  chan Event
	done    chan struct{}
	once    sync.Once
	request int
}

func New(binary string) (*Player, error) {
	if binary == "" {
		binary = "mpv"
	}
	path, err := exec.LookPath(binary)
	if err != nil {
		return nil, errors.New("mpv not found; install with brew install mpv")
	}
	dir, err := os.MkdirTemp("", "akari-mpv-")
	if err != nil {
		return nil, err
	}
	socket := filepath.Join(dir, "ipc")
	cmd := exec.Command(path, "--no-config", "--idle=yes", "--no-video", "--audio-display=no", "--terminal=no", "--input-default-bindings=no", "--input-ipc-server="+socket, "--keep-open=no", "--volume=70", "--cache=yes")
	cmd.Stdout = nil
	cmd.Stderr = nil
	if err = cmd.Start(); err != nil {
		os.RemoveAll(dir)
		return nil, errors.New("cannot start mpv")
	}
	p := &Player{cmd: cmd, dir: dir, events: make(chan Event, 128), done: make(chan struct{})}
	go func() { _ = cmd.Wait(); close(p.done) }()
	for range 500 {
		p.conn, err = net.DialTimeout("unix", socket, 100*time.Millisecond)
		if err == nil {
			break
		}
		select {
		case <-p.done:
			os.RemoveAll(dir)
			return nil, errors.New("mpv exited during startup")
		default:
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err != nil {
		p.Close()
		return nil, errors.New("cannot connect to mpv")
	}
	go p.read()
	for i, name := range []string{"time-pos", "duration", "pause", "paused-for-cache", "eof-reached"} {
		_ = p.Command("observe_property", i+1, name)
	}
	return p, nil
}
func (p *Player) read() {
	defer close(p.events)
	scanner := bufio.NewScanner(p.conn)
	scanner.Buffer(make([]byte, 4096), 1<<20)
	for scanner.Scan() {
		var r reply
		if json.Unmarshal(scanner.Bytes(), &r) != nil {
			continue
		}
		e := Event{}
		switch r.Event {
		case "property-change":
			e = Event{Name: r.Name, Value: r.Data}
		case "end-file":
			e = Event{Name: "end-file", Value: r.Reason}
			if r.Reason == "error" {
				e.Error = "Playback failed"
			}
		case "file-loaded":
			e.Name = "file-loaded"
		case "start-file":
			e.Name = "start-file"
		default:
			if r.Error != "" && r.Error != "success" {
				e = Event{Name: "error", Error: "Player: " + r.Error}
			} else {
				continue
			}
		}
		select {
		case p.events <- e:
		case <-p.done:
			return
		}
	}
}
func (p *Player) Events() <-chan Event { return p.events }
func (p *Player) Command(args ...any) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.conn == nil {
		return errors.New("player unavailable")
	}
	p.request++
	b, _ := json.Marshal(map[string]any{"command": args, "request_id": p.request})
	_ = p.conn.SetWriteDeadline(time.Now().Add(2 * time.Second))
	_, err := p.conn.Write(append(b, '\n'))
	if err != nil {
		return errors.New("player disconnected")
	}
	return nil
}
func (p *Player) Load(source string, paused bool, position float64) error {
	opts := fmt.Sprintf("pause=%s,start=%.3f", map[bool]string{true: "yes", false: "no"}[paused], max(0, position))
	return p.Command("loadfile", source, "replace", -1, opts)
}
func (p *Player) Pause(paused bool) error { return p.Command("set_property", "pause", paused) }
func (p *Player) Seek(seconds float64, absolute bool) error {
	mode := "relative+exact"
	if absolute {
		mode = "absolute+exact"
	}
	return p.Command("seek", seconds, mode)
}
func (p *Player) Volume(v float64) error {
	return p.Command("set_property", "volume", min(100, max(0, v)))
}
func (p *Player) Stop() error { return p.Command("stop") }
func (p *Player) Close() error {
	p.once.Do(func() {
		if p.conn != nil {
			_ = p.Command("quit")
			_ = p.conn.Close()
		}
		if p.cmd != nil {
			select {
			case <-p.done:
			case <-time.After(time.Second):
				_ = p.cmd.Process.Kill()
				<-p.done
			}
		}
		_ = os.RemoveAll(p.dir)
	})
	return nil
}
