package library

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

const (
	subsonicPageSize = 500
	maxResponseSize  = 64 << 20
)

type subsonicEnvelope struct {
	Response subsonicResponse `json:"subsonic-response"`
}

type subsonicResponse struct {
	Status    string               `json:"status"`
	AlbumList *subsonicAlbumList   `json:"albumList2"`
	Album     *subsonicAlbumDetail `json:"album"`
	Error     *subsonicError       `json:"error"`
}

type subsonicError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type subsonicAlbumList struct {
	Albums oneOrMany[subsonicAlbum] `json:"album"`
}

type subsonicAlbum struct {
	ID       string         `json:"id"`
	Name     string         `json:"name"`
	Artist   string         `json:"artist"`
	Genre    string         `json:"genre"`
	Year     *int           `json:"year"`
	CoverArt string         `json:"coverArt"`
	Played   subsonicPlayed `json:"played"`
}

type subsonicAlbumDetail struct {
	Name     string                  `json:"name"`
	CoverArt string                  `json:"coverArt"`
	Songs    oneOrMany[subsonicSong] `json:"song"`
}

type subsonicSong struct {
	ID         string   `json:"id"`
	Title      string   `json:"title"`
	Artist     string   `json:"artist"`
	Album      string   `json:"album"`
	CoverArt   string   `json:"coverArt"`
	Duration   *float64 `json:"duration"`
	Track      *int     `json:"track"`
	DiscNumber *int     `json:"discNumber"`
}

type subsonicPlayed struct {
	Date string `json:"date"`
}

type oneOrMany[T any] []T

func (items *oneOrMany[T]) UnmarshalJSON(data []byte) error {
	if string(data) == "null" {
		*items = nil
		return nil
	}
	var many []T
	if err := json.Unmarshal(data, &many); err == nil {
		*items = many
		return nil
	}
	var one T
	if err := json.Unmarshal(data, &one); err != nil {
		return err
	}
	*items = []T{one}
	return nil
}

func (played *subsonicPlayed) UnmarshalJSON(data []byte) error {
	if string(data) == "null" {
		return nil
	}
	if len(data) > 0 && data[0] == '"' {
		return json.Unmarshal(data, &played.Date)
	}
	var value struct {
		Date string `json:"date"`
	}
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	played.Date = value.Date
	return nil
}

func (l *Library) fetchAlbums(ctx context.Context) ([]cachedAlbum, error) {
	const pageSize = subsonicPageSize

	albums := make([]cachedAlbum, 0)
	for offset := 0; ; offset += pageSize {
		if err := ctx.Err(); err != nil {
			return nil, fmt.Errorf("request canceled: %w", err)
		}
		values := map[string]string{
			"type":   "alphabeticalByName",
			"size":   strconv.Itoa(pageSize),
			"offset": strconv.Itoa(offset),
			"f":      "json",
		}
		var envelope subsonicEnvelope
		if err := l.getSubsonic(ctx, "getAlbumList2", values, &envelope); err != nil {
			return nil, err
		}
		if err := l.checkSubsonicResponse(envelope.Response); err != nil {
			return nil, err
		}
		if envelope.Response.AlbumList == nil {
			return nil, errors.New("server returned an invalid album list")
		}

		page := envelope.Response.AlbumList.Albums
		for _, item := range page {
			if item.ID == "" {
				continue
			}
			albums = append(albums, cachedAlbum{
				ID:         item.ID,
				Title:      item.Name,
				Artist:     item.Artist,
				Genre:      strings.TrimSpace(item.Genre),
				Year:       intValue(item.Year),
				LastPlayed: parsePlayed(item.Played.Date),
			})
		}
		if len(page) < pageSize {
			break
		}
	}
	return albums, nil
}

func (l *Library) fetchSongs(ctx context.Context, albumID string) ([]cachedSong, *cachedAlbum, error) {
	var envelope subsonicEnvelope
	if err := l.getSubsonic(ctx, "getAlbum", map[string]string{
		"id": albumID,
		"f":  "json",
	}, &envelope); err != nil {
		return nil, nil, err
	}
	if err := l.checkSubsonicResponse(envelope.Response); err != nil {
		return nil, nil, err
	}
	if envelope.Response.Album == nil {
		return nil, nil, errors.New("server returned an invalid album")
	}

	detail := envelope.Response.Album
	songs := make([]cachedSong, 0, len(detail.Songs))
	artists := make([]string, 0, len(detail.Songs))
	for _, item := range detail.Songs {
		if item.ID == "" {
			continue
		}
		albumTitle := item.Album
		if albumTitle == "" {
			albumTitle = detail.Name
		}
		songs = append(songs, cachedSong{
			ID:          item.ID,
			Title:       item.Title,
			Artist:      item.Artist,
			AlbumID:     albumID,
			AlbumTitle:  albumTitle,
			Duration:    floatValue(item.Duration),
			TrackNumber: intValue(item.Track),
			DiscNumber:  intValue(item.DiscNumber),
		})
		artists = append(artists, item.Artist)
	}

	album := &cachedAlbum{
		ID:     albumID,
		Title:  detail.Name,
		Artist: firstNonEmpty(artists),
	}
	if album.Title == "" && len(songs) > 0 {
		album.Title = songs[0].AlbumTitle
	}
	if album.Artist == "" && len(songs) > 0 {
		album.Artist = songs[0].Artist
	}
	return songs, album, nil
}

func (l *Library) getSubsonic(ctx context.Context, action string, values map[string]string, target any) error {
	requestURL, err := l.endpoint(action, values)
	if err != nil {
		return err
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, requestURL, nil)
	if err != nil {
		return errors.New("build server request failed")
	}
	response, err := l.client.Do(request)
	if err != nil {
		return l.safeTransportError(ctx, err)
	}
	defer response.Body.Close()

	if response.StatusCode < http.StatusOK || response.StatusCode >= http.StatusMultipleChoices {
		return fmt.Errorf("server returned HTTP %d", response.StatusCode)
	}
	limited := io.LimitReader(response.Body, maxResponseSize+1)
	data, err := io.ReadAll(limited)
	if err != nil {
		return errors.New("read server response failed")
	}
	if len(data) > maxResponseSize {
		return errors.New("server response is too large")
	}
	if err := json.Unmarshal(data, target); err != nil {
		return errors.New("server returned an invalid JSON response")
	}
	return nil
}

func (l *Library) checkSubsonicResponse(response subsonicResponse) error {
	if response.Status == "ok" {
		return nil
	}
	if response.Error != nil {
		message := strings.TrimSpace(response.Error.Message)
		if response.Error.Code == 40 || response.Error.Code == 50 {
			return errors.New("server rejected the username or password")
		}
		if message != "" {
			return fmt.Errorf("server error %d: %s", response.Error.Code, l.redact(message))
		}
		return fmt.Errorf("server error %d", response.Error.Code)
	}
	return errors.New("server returned a failed response")
}

func (l *Library) safeTransportError(ctx context.Context, err error) error {
	if ctxErr := ctx.Err(); ctxErr != nil {
		return fmt.Errorf("request canceled: %w", ctxErr)
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return errors.New("server request timed out")
	}
	var dnsErr *net.DNSError
	if errors.As(err, &dnsErr) {
		return errors.New("server host could not be resolved")
	}
	return errors.New("server could not be reached")
}

func (l *Library) redact(value string) string {
	value = strings.TrimSpace(value)
	secrets := []string{
		l.config.Server.Password,
		l.config.Server.Username,
		l.authSalt,
		l.authToken,
		l.config.Server.URL,
	}
	for _, secret := range secrets {
		if secret != "" {
			value = strings.ReplaceAll(value, secret, "[redacted]")
		}
	}
	const maxLength = 240
	if len(value) > maxLength {
		value = value[:maxLength] + "..."
	}
	if value == "" {
		return "server request failed"
	}
	return value
}

func (l *Library) endpoint(action string, values map[string]string) (string, error) {
	if !l.hasServer() {
		return "", errNoServer
	}
	base, err := url.Parse(l.config.Server.URL)
	if err != nil || base.Host == "" || (base.Scheme != "http" && base.Scheme != "https") {
		return "", errors.New("server URL must be an absolute HTTP or HTTPS URL")
	}

	base.Path = strings.TrimRight(base.Path, "/") + "/rest/" + action + ".view"
	base.RawPath = ""
	base.Fragment = ""
	query := base.Query()
	query.Set("u", l.config.Server.Username)
	query.Set("t", l.authToken)
	query.Set("s", l.authSalt)
	query.Set("v", "1.16.1")
	query.Set("c", "Akari")
	for key, value := range values {
		query.Set(key, value)
	}
	base.RawQuery = query.Encode()
	return base.String(), nil
}

func parsePlayed(value string) float64 {
	value = strings.TrimSpace(value)
	if value == "" {
		return 0
	}
	parsed, err := time.Parse(time.RFC3339Nano, value)
	if err != nil {
		return 0
	}
	return float64(parsed.UnixNano()) / float64(time.Second)
}

func firstNonEmpty(values []string) string {
	for _, value := range values {
		if value = strings.TrimSpace(value); value != "" {
			return value
		}
	}
	return ""
}

func floatValue(value *float64) float64 {
	if value == nil {
		return 0
	}
	return *value
}
