package library

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"akari/internal/core"

	"github.com/pelletier/go-toml/v2"
)

// LoadConfig loads Akari's TOML configuration, creating a default file when
// one does not exist. It returns the path that was read or created.
func LoadConfig() (core.Config, string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return core.Config{}, "", fmt.Errorf("determine home directory: %w", err)
	}

	configRoot := os.Getenv("XDG_CONFIG_HOME")
	if configRoot == "" {
		configRoot = filepath.Join(home, ".config")
	} else {
		configRoot = expandHome(configRoot, home)
	}
	configPath := filepath.Join(configRoot, "akari", "config.toml")

	cfg := defaultConfig(home)
	created, err := createDefaultConfig(configPath, cfg)
	if err != nil {
		return core.Config{}, configPath, err
	}
	if created {
		return cfg, configPath, fmt.Errorf(
			"Created %s; configure [server] or [storage].external_dirs, then run akari",
			configPath,
		)
	}

	data, err := os.ReadFile(configPath)
	if err != nil {
		return core.Config{}, configPath, fmt.Errorf("read config %q: %w", configPath, err)
	}
	if err := toml.Unmarshal(data, &cfg); err != nil {
		return core.Config{}, configPath, fmt.Errorf("parse config %q: invalid TOML configuration", configPath)
	}

	if strings.TrimSpace(cfg.Storage.MusicDir) == "" {
		cfg.Storage.MusicDir = defaultMusicDir(home)
	} else {
		cfg.Storage.MusicDir = expandHome(cfg.Storage.MusicDir, home)
		if !filepath.IsAbs(cfg.Storage.MusicDir) {
			cfg.Storage.MusicDir = filepath.Join(filepath.Dir(configPath), cfg.Storage.MusicDir)
		}
	}
	for i, dir := range cfg.Storage.ExternalDirs {
		cfg.Storage.ExternalDirs[i] = expandHome(dir, home)
		if !filepath.IsAbs(cfg.Storage.ExternalDirs[i]) {
			cfg.Storage.ExternalDirs[i] = filepath.Join(filepath.Dir(configPath), cfg.Storage.ExternalDirs[i])
		}
	}

	return cfg, configPath, nil
}

func defaultConfig(home string) core.Config {
	var cfg core.Config
	cfg.Storage.MusicDir = defaultMusicDir(home)
	cfg.Player.SpatialAudio = true
	cfg.Storage.ExternalDirs = []string{}
	return cfg
}

func defaultMusicDir(home string) string {
	return filepath.Join(home, ".local", "share", "akari", "music")
}

func createDefaultConfig(path string, cfg core.Config) (bool, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return false, fmt.Errorf("create config directory for %q: %w", path, err)
	}

	data, err := toml.Marshal(cfg)
	if err != nil {
		return false, fmt.Errorf("build default config: %w", err)
	}
	data = append(data, '\n')

	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, os.ErrExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("create config %q: %w", path, err)
	}

	ok := false
	defer func() {
		_ = file.Close()
		if !ok {
			_ = os.Remove(path)
		}
	}()

	if _, err := file.Write(data); err != nil {
		return false, fmt.Errorf("write config %q: %w", path, err)
	}
	if err := file.Sync(); err != nil {
		return false, fmt.Errorf("sync config %q: %w", path, err)
	}
	if err := file.Close(); err != nil {
		return false, fmt.Errorf("close config %q: %w", path, err)
	}
	ok = true
	return true, nil
}

func expandHome(path, home string) string {
	switch {
	case path == "~":
		return home
	case strings.HasPrefix(path, "~/"), strings.HasPrefix(path, `~\`):
		return filepath.Join(home, path[2:])
	default:
		return path
	}
}
