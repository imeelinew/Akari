package core

type Album struct {
	ID         string  `json:"id"`
	Title      string  `json:"title"`
	Artist     string  `json:"artist"`
	Genre      string  `json:"genre,omitempty"`
	Year       int     `json:"year,omitempty"`
	ArtworkURL string  `json:"artworkURL,omitempty"`
	LastPlayed float64 `json:"lastPlayed,omitempty"`
}
type Song struct {
	ID          string  `json:"id"`
	Title       string  `json:"title"`
	Artist      string  `json:"artist"`
	AlbumID     string  `json:"albumID"`
	AlbumTitle  string  `json:"albumTitle"`
	Duration    float64 `json:"duration"`
	TrackNumber int     `json:"trackNumber,omitempty"`
	DiscNumber  int     `json:"discNumber,omitempty"`
	ArtworkURL  string  `json:"artworkURL,omitempty"`
}
type Config struct {
	Server struct {
		URL      string `toml:"url"`
		Username string `toml:"username"`
		Password string `toml:"password"`
	} `toml:"server"`
	Storage struct {
		MusicDir     string   `toml:"music_dir"`
		ExternalDirs []string `toml:"external_dirs"`
	} `toml:"storage"`
	Player struct {
		MPV string `toml:"mpv"`
	} `toml:"player"`
}
