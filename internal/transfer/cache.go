package transfer

import (
	"crypto/sha256"
	"encoding/hex"
	"mime"
	"net/url"
	"path"
	"strings"
)

type byteRange struct {
	start int64
	end   int64
}

func cacheFileName(songID string) string {
	sum := sha256.Sum256([]byte(songID))
	return hex.EncodeToString(sum[:]) + ".part"
}

func addCoverage(ranges []byteRange, start, end int64) []byteRange {
	if end < start {
		return ranges
	}
	next := byteRange{start: start, end: end}
	result := make([]byteRange, 0, len(ranges)+1)
	inserted := false
	for _, current := range ranges {
		if current.end+1 < next.start {
			result = append(result, current)
			continue
		}
		if next.end+1 < current.start {
			if !inserted {
				result = append(result, next)
				inserted = true
			}
			result = append(result, current)
			continue
		}
		if current.start < next.start {
			next.start = current.start
		}
		if current.end > next.end {
			next.end = current.end
		}
	}
	if !inserted {
		result = append(result, next)
	}
	return result
}

func coveredBytes(ranges []byteRange) int64 {
	var total int64
	for _, current := range ranges {
		total += current.end - current.start + 1
	}
	return total
}

func containsRange(ranges []byteRange, start, end int64) bool {
	if end < start {
		return false
	}
	for _, current := range ranges {
		if current.start <= start && current.end >= end {
			return true
		}
	}
	return false
}

func missingChunk(ranges []byteRange, start, end int64) (int64, int64, bool) {
	if end < start {
		return 0, 0, false
	}
	for _, current := range ranges {
		if current.end < start {
			continue
		}
		if current.start > start {
			holeEnd := current.start - 1
			if holeEnd > end {
				holeEnd = end
			}
			return start, holeEnd, true
		}
		if current.end >= end {
			return 0, 0, false
		}
		start = current.end + 1
	}
	return start, end, start <= end
}

func chooseExtension(contentType, originalExtension string) string {
	mediaType, _, err := mime.ParseMediaType(contentType)
	if err == nil {
		switch strings.ToLower(mediaType) {
		case "audio/mpeg", "audio/mp3":
			return ".mp3"
		case "audio/mp4", "audio/x-m4a", "audio/m4a":
			return ".m4a"
		case "audio/aac", "audio/aacp":
			return ".aac"
		case "audio/flac", "audio/x-flac":
			return ".flac"
		case "audio/ogg", "application/ogg":
			return ".ogg"
		case "audio/opus":
			return ".opus"
		case "audio/wav", "audio/wave", "audio/x-wav":
			return ".wav"
		case "audio/webm":
			return ".weba"
		case "audio/x-ms-wma":
			return ".wma"
		case "audio/ape", "audio/x-ape":
			return ".ape"
		}
	}
	if originalExtension != "" {
		if extension := supportedAudioExtension(originalExtension); extension != "" {
			return extension
		}
	}
	return ".bin"
}

func extensionFromURL(raw string) string {
	u, err := url.Parse(raw)
	if err != nil {
		return ""
	}
	return supportedAudioExtension(path.Ext(u.Path))
}

func supportedAudioExtension(extension string) string {
	ext := strings.ToLower(strings.TrimSpace(extension))
	switch ext {
	case ".aac", ".aif", ".aiff", ".alac", ".ape", ".flac", ".m4a", ".mp3",
		".mp4", ".oga", ".ogg", ".opus", ".wav", ".weba", ".webm", ".wma":
		return ext
	default:
		return ""
	}
}
