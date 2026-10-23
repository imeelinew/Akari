package artwork

import (
	"bytes"
	"encoding/base64"
	"fmt"
	"image/png"
	"os"
	"sort"
	"strings"
	"sync"
)

const (
	graphicsChunkBytes  = 3072
	maxGraphicsPNGBytes = 4 << 20
)

var graphicsState = struct {
	sync.Mutex
	images map[uint32]struct{}
}{
	images: make(map[uint32]struct{}),
}

// Supported reports whether the current environment exposes Kitty graphics
// through Ghostty, Rex, or kitty.
func Supported() bool {
	if strings.TrimSpace(os.Getenv("TMUX")) != "" {
		return false
	}

	program := strings.ToLower(strings.TrimSpace(os.Getenv("TERM_PROGRAM")))
	if program == "ghostty" || program == "rex" {
		return true
	}

	term := strings.ToLower(strings.TrimSpace(os.Getenv("TERM")))
	return strings.Contains(term, "ghostty") ||
		term == "xterm-rex" ||
		strings.Contains(term, "kitty") ||
		strings.TrimSpace(os.Getenv("KITTY_WINDOW_ID")) != ""
}

// Render returns a cursor-preserving Kitty sequence that uploads data once and
// places it inside the requested cell bounds.
func Render(data []byte, imageID uint32, column, row, columns, rows int) string {
	if len(data) == 0 || len(data) > maxGraphicsPNGBytes || imageID == 0 ||
		column <= 0 || row <= 0 || columns <= 0 || rows <= 0 {
		return ""
	}
	if _, err := png.DecodeConfig(bytes.NewReader(data)); err != nil {
		return ""
	}

	encodedBytes := base64.StdEncoding.EncodedLen(len(data))
	chunks := (len(data) + graphicsChunkBytes - 1) / graphicsChunkBytes
	var output strings.Builder
	output.Grow(encodedBytes + chunks*40 + 96)
	output.WriteString("\x1b7")
	fmt.Fprintf(&output, "\x1b[%d;%dH", row, column)

	var encoded [4096]byte
	for offset := 0; offset < len(data); {
		end := min(offset+graphicsChunkBytes, len(data))
		encodedSize := base64.StdEncoding.EncodedLen(end - offset)
		base64.StdEncoding.Encode(encoded[:encodedSize], data[offset:end])
		more := 0
		if end < len(data) {
			more = 1
		}
		if offset == 0 {
			fmt.Fprintf(&output, "\x1b_Ga=t,t=d,f=100,i=%d,q=2,m=%d;", imageID, more)
		} else {
			fmt.Fprintf(&output, "\x1b_Gm=%d,q=2;", more)
		}
		output.Write(encoded[:encodedSize])
		output.WriteString("\x1b\\")
		clear(encoded[:encodedSize])
		offset = end
	}
	clear(encoded[:])

	writePlacement(&output, imageID, columns, rows)
	output.WriteString("\x1b8")
	trackImage(imageID)
	return output.String()
}

// Place returns a cursor-preserving Kitty sequence that displays an already
// uploaded image inside the requested cell bounds.
func Place(imageID uint32, column, row, columns, rows int) string {
	if imageID == 0 || column <= 0 || row <= 0 || columns <= 0 || rows <= 0 {
		return ""
	}
	var output strings.Builder
	output.Grow(112)
	output.WriteString("\x1b7")
	fmt.Fprintf(&output, "\x1b[%d;%dH", row, column)
	writePlacement(&output, imageID, columns, rows)
	output.WriteString("\x1b8")
	return output.String()
}

// Delete returns a sequence that removes every placement of imageID while
// keeping the uploaded image data available for Place.
func Delete(imageID uint32) string {
	if imageID == 0 {
		return ""
	}
	return fmt.Sprintf("\x1b_Ga=d,d=i,i=%d,q=2\x1b\\", imageID)
}

// Clear deletes uploaded image data created through Render by this process.
func Clear() string {
	graphicsState.Lock()
	ids := make([]int, 0, len(graphicsState.images))
	for imageID := range graphicsState.images {
		ids = append(ids, int(imageID))
	}
	clear(graphicsState.images)
	graphicsState.Unlock()

	sort.Ints(ids)
	var output strings.Builder
	output.Grow(len(ids) * 48)
	for _, id := range ids {
		fmt.Fprintf(&output, "\x1b_Ga=d,d=I,i=%d,q=2\x1b\\", id)
	}
	return output.String()
}

func writePlacement(output *strings.Builder, imageID uint32, columns, rows int) {
	fmt.Fprintf(output, "\x1b_Ga=p,i=%d,c=%d,r=%d,C=1,q=2,z=-1\x1b\\",
		imageID, columns, rows)
}

func trackImage(imageID uint32) {
	graphicsState.Lock()
	graphicsState.images[imageID] = struct{}{}
	graphicsState.Unlock()
}

// Forget releases both placement and data for one image owned by Akari.
func Forget(imageID uint32) string {
	if imageID == 0 {
		return ""
	}
	graphicsState.Lock()
	delete(graphicsState.images, imageID)
	graphicsState.Unlock()
	return fmt.Sprintf("\x1b_Ga=d,d=I,i=%d,q=2\x1b\\", imageID)
}
