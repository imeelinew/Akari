package main

import (
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"syscall"

	"akari/internal/library"
	"akari/internal/player"
	"akari/internal/transfer"
	"akari/internal/ui"
	tea "charm.land/bubbletea/v2"
	"golang.org/x/term"
)

var version = "0.1.0"

func run() error {
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "--version", "-v":
			fmt.Println("Akari " + version)
			return nil
		case "--help", "-h":
			fmt.Println("Akari\n\nUsage: akari\nConfig: ~/.config/akari/config.toml")
			return nil
		default:
			return fmt.Errorf("unknown option: %s", os.Args[1])
		}
	}
	if runtime.GOOS != "darwin" {
		return fmt.Errorf("Akari requires macOS")
	}
	cfg, _, err := library.LoadConfig()
	if err != nil {
		return err
	}
	if !term.IsTerminal(int(os.Stdin.Fd())) || !term.IsTerminal(int(os.Stdout.Fd())) {
		return fmt.Errorf("interactive terminal required")
	}
	lib, err := library.New(cfg)
	if err != nil {
		return err
	}
	defer lib.Close()
	p, err := player.New(cfg.Player.SpatialAudio)
	if err != nil {
		return err
	}
	t, err := transfer.New(lib)
	if err != nil {
		p.Close()
		return err
	}
	model := ui.New(lib, p, t)
	defer model.Close()
	program := tea.NewProgram(model)
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(signals)
	done := make(chan struct{})
	defer close(done)
	go func() {
		select {
		case <-signals:
			program.Quit()
		case <-done:
		}
	}()
	_, err = program.Run()
	return err
}
func main() {
	if err := player.Run(run); err != nil {
		fmt.Fprintln(os.Stderr, "akari:", err)
		os.Exit(1)
	}
}
