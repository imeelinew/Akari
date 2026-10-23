package artwork

import (
	"bytes"
	"container/list"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"image"
	"image/color"
	_ "image/gif"
	_ "image/jpeg"
	"image/png"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	maxImagePixels     = 16_000_000
	maxInputBytes      = 12 << 20
	maxInputDimension  = 20_000
	maxOutputDimension = 700
	maxOutputBytes     = 4 << 20
	memoryCacheItems   = 64
	httpTimeout        = 15 * time.Second
)

// Cache loads, normalizes, and caches artwork.
type Cache struct {
	cacheDir string
	client   *http.Client

	mu    sync.Mutex
	items map[string]*list.Element
	lru   *list.List
}

type cacheItem struct {
	key  string
	data []byte
}

type contextReader struct {
	ctx context.Context
	r   io.Reader
}

func (r contextReader) Read(p []byte) (int, error) {
	if err := r.ctx.Err(); err != nil {
		return 0, err
	}
	n, err := r.r.Read(p)
	if ctxErr := r.ctx.Err(); ctxErr != nil {
		return n, ctxErr
	}
	return n, err
}

type boundedBuffer struct {
	bytes.Buffer
	limit int
}

func (b *boundedBuffer) Write(p []byte) (int, error) {
	if len(p) > b.limit-b.Len() {
		return 0, fmt.Errorf("artwork: encoded image exceeds %d bytes", b.limit)
	}
	return b.Buffer.Write(p)
}

// New creates an artwork cache rooted at cacheDir. An empty cacheDir keeps the
// cache in memory only.
func New(cacheDir string) *Cache {
	if cacheDir != "" {
		cacheDir = filepath.Clean(cacheDir)
	}
	return &Cache{
		cacheDir: cacheDir,
		client: &http.Client{
			Timeout: httpTimeout,
		},
		items: make(map[string]*list.Element),
		lru:   list.New(),
	}
}

// Load returns PNG artwork from the memory or disk cache. On a miss it reads
// source, decodes its first JPEG, PNG, or GIF frame, scales its longest edge to
// at most 700 pixels, and stores the encoded PNG.
func (c *Cache) Load(ctx context.Context, key, source string) ([]byte, error) {
	if ctx == nil {
		return nil, errors.New("artwork: nil context")
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	hash := sha256.Sum256([]byte(key))
	cacheKey := hex.EncodeToString(hash[:])
	if data, ok := c.memoryGet(cacheKey); ok {
		return data, nil
	}

	if data, ok := c.diskGet(cacheKey); ok {
		c.memoryPut(cacheKey, data)
		return bytes.Clone(data), nil
	}

	raw, err := c.readSource(ctx, source)
	if err != nil {
		return nil, err
	}

	data, err := normalize(ctx, raw)
	clear(raw)
	if err != nil {
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		clear(data)
		return nil, err
	}

	c.memoryPut(cacheKey, data)
	_ = c.diskPut(cacheKey, data)
	return bytes.Clone(data), nil
}

func (c *Cache) readSource(ctx context.Context, source string) ([]byte, error) {
	parsed, parseErr := url.Parse(source)
	if parseErr == nil && parsed.Scheme != "" {
		switch strings.ToLower(parsed.Scheme) {
		case "http", "https":
			if parsed.Host == "" {
				return nil, errors.New("artwork: HTTP source has no host")
			}
			return c.readHTTP(ctx, source)
		default:
			return nil, fmt.Errorf("artwork: unsupported source scheme %q", parsed.Scheme)
		}
	}

	if !filepath.IsAbs(source) {
		return nil, errors.New("artwork: local source must be an absolute path")
	}
	return readFile(ctx, source)
}

func (c *Cache) readHTTP(ctx context.Context, source string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, source, nil)
	if err != nil {
		return nil, fmt.Errorf("artwork: create request: %w", err)
	}
	req.Header.Set("Accept", "image/jpeg,image/png,image/gif")

	resp, err := c.client.Do(req)
	if err != nil {
		return nil, errors.New("artwork: request failed")
	}
	defer resp.Body.Close()
	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 64<<10))
		return nil, fmt.Errorf("artwork: HTTP %s", resp.Status)
	}
	if resp.ContentLength > maxInputBytes {
		return nil, fmt.Errorf("artwork: source exceeds %d bytes", maxInputBytes)
	}
	return readLimited(ctx, resp.Body, maxInputBytes)
}

func readFile(ctx context.Context, path string) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("artwork: open %s: %w", path, err)
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("artwork: stat %s: %w", path, err)
	}
	if !info.Mode().IsRegular() {
		return nil, fmt.Errorf("artwork: source %s is not a regular file", path)
	}
	if info.Size() > maxInputBytes {
		return nil, fmt.Errorf("artwork: source exceeds %d bytes", maxInputBytes)
	}
	return readLimited(ctx, file, maxInputBytes)
}

func readLimited(ctx context.Context, r io.Reader, limit int64) ([]byte, error) {
	data, err := io.ReadAll(io.LimitReader(contextReader{ctx: ctx, r: r}, limit+1))
	if err != nil {
		clear(data)
		return nil, fmt.Errorf("artwork: read source: %w", err)
	}
	if int64(len(data)) > limit {
		clear(data)
		return nil, fmt.Errorf("artwork: source exceeds %d bytes", limit)
	}
	return data, nil
}

func normalize(ctx context.Context, raw []byte) ([]byte, error) {
	config, format, err := image.DecodeConfig(bytes.NewReader(raw))
	if err != nil {
		return nil, fmt.Errorf("artwork: decode image header: %w", err)
	}
	switch format {
	case "gif", "jpeg", "png":
	default:
		return nil, fmt.Errorf("artwork: unsupported image format %q", format)
	}
	if err := validateDimensions(config.Width, config.Height); err != nil {
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	source, _, err := image.Decode(bytes.NewReader(raw))
	if err != nil {
		return nil, fmt.Errorf("artwork: decode %s image: %w", format, err)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	bounds := source.Bounds()
	width, height := normalizedSize(bounds.Dx(), bounds.Dy())
	if width != bounds.Dx() || height != bounds.Dy() {
		source = scaleDown(ctx, source, width, height)
		if err := ctx.Err(); err != nil {
			return nil, err
		}
	}

	var output boundedBuffer
	output.limit = maxOutputBytes
	encoder := png.Encoder{CompressionLevel: png.DefaultCompression}
	if err := encoder.Encode(&output, source); err != nil {
		clear(output.Bytes())
		return nil, fmt.Errorf("artwork: encode PNG: %w", err)
	}
	data := bytes.Clone(output.Bytes())
	clear(output.Bytes())
	return data, nil
}

func validateDimensions(width, height int) error {
	if width <= 0 || height <= 0 {
		return errors.New("artwork: image has invalid dimensions")
	}
	if width > maxInputDimension || height > maxInputDimension {
		return fmt.Errorf("artwork: image dimensions %dx%d exceed limit", width, height)
	}
	if width > maxImagePixels/height {
		return fmt.Errorf("artwork: image contains more than %d pixels", maxImagePixels)
	}
	return nil
}

func normalizedSize(width, height int) (int, int) {
	longest := max(width, height)
	if longest <= maxOutputDimension {
		return width, height
	}
	scaledWidth := max(1, (width*maxOutputDimension+longest/2)/longest)
	scaledHeight := max(1, (height*maxOutputDimension+longest/2)/longest)
	return scaledWidth, scaledHeight
}

func scaleDown(ctx context.Context, source image.Image, width, height int) image.Image {
	destination := image.NewRGBA(image.Rect(0, 0, width, height))
	bounds := source.Bounds()
	sourceWidth := bounds.Dx()
	sourceHeight := bounds.Dy()
	cellCount := width * height
	sums := make([]uint64, cellCount*5)

	for y := 0; y < sourceHeight; y++ {
		if y&31 == 0 && ctx.Err() != nil {
			return destination
		}
		destinationY := y * height / sourceHeight
		rowOffset := destinationY * width
		for x := 0; x < sourceWidth; x++ {
			destinationX := x * width / sourceWidth
			offset := (rowOffset + destinationX) * 5
			r, g, b, a := source.At(bounds.Min.X+x, bounds.Min.Y+y).RGBA()
			sums[offset] += uint64(r)
			sums[offset+1] += uint64(g)
			sums[offset+2] += uint64(b)
			sums[offset+3] += uint64(a)
			sums[offset+4]++
		}
	}

	offset := 0
	for y := 0; y < height; y++ {
		for x := 0; x < width; x++ {
			count := sums[offset+4]
			if count == 0 {
				offset += 5
				continue
			}
			destination.SetRGBA(x, y, imageColor(
				sums[offset]/count,
				sums[offset+1]/count,
				sums[offset+2]/count,
				sums[offset+3]/count,
			))
			offset += 5
		}
	}
	return destination
}

func (c *Cache) memoryGet(key string) ([]byte, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	element, ok := c.items[key]
	if !ok {
		return nil, false
	}
	c.lru.MoveToFront(element)
	return bytes.Clone(element.Value.(*cacheItem).data), true
}

func (c *Cache) memoryPut(key string, data []byte) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if element, ok := c.items[key]; ok {
		item := element.Value.(*cacheItem)
		item.data = bytes.Clone(data)
		c.lru.MoveToFront(element)
		return
	}

	element := c.lru.PushFront(&cacheItem{key: key, data: bytes.Clone(data)})
	c.items[key] = element
	for c.lru.Len() > memoryCacheItems {
		oldest := c.lru.Back()
		item := oldest.Value.(*cacheItem)
		delete(c.items, item.key)
		clear(item.data)
		c.lru.Remove(oldest)
	}
}

func (c *Cache) diskGet(key string) ([]byte, bool) {
	if c.cacheDir == "" {
		return nil, false
	}
	path := filepath.Join(c.cacheDir, key+".png")
	file, err := os.Open(path)
	if err != nil {
		return nil, false
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0o077 != 0 ||
		info.Size() <= 0 || info.Size() > maxOutputBytes {
		_ = os.Remove(path)
		return nil, false
	}
	data, err := io.ReadAll(io.LimitReader(file, maxOutputBytes+1))
	if err != nil || len(data) > maxOutputBytes {
		clear(data)
		_ = os.Remove(path)
		return nil, false
	}
	if err := validateCachedPNG(data); err != nil {
		clear(data)
		_ = os.Remove(path)
		return nil, false
	}
	return data, true
}

func validateCachedPNG(data []byte) error {
	config, err := png.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return err
	}
	if config.Width <= 0 || config.Height <= 0 ||
		config.Width > maxOutputDimension || config.Height > maxOutputDimension {
		return errors.New("artwork: invalid cached PNG dimensions")
	}
	_, err = png.Decode(bytes.NewReader(data))
	return err
}

func (c *Cache) diskPut(key string, data []byte) error {
	if c.cacheDir == "" {
		return nil
	}
	if err := ensureCacheDir(c.cacheDir); err != nil {
		return err
	}

	path := filepath.Join(c.cacheDir, key+".png")
	file, err := os.CreateTemp(c.cacheDir, ".artwork-*.tmp")
	if err != nil {
		return err
	}
	tempPath := file.Name()
	renamed := false
	defer func() {
		_ = file.Close()
		if !renamed {
			_ = os.Remove(tempPath)
		}
	}()

	if err := file.Chmod(0o600); err != nil {
		return err
	}
	if _, err := file.Write(data); err != nil {
		return err
	}
	if err := file.Sync(); err != nil {
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := os.Rename(tempPath, path); err != nil {
		return err
	}
	renamed = true

	directory, err := os.Open(c.cacheDir)
	if err != nil {
		return err
	}
	defer directory.Close()
	return directory.Sync()
}

func ensureCacheDir(path string) error {
	if err := os.MkdirAll(path, 0o700); err != nil {
		return err
	}
	return os.Chmod(path, 0o700)
}

func imageColor(r, g, b, a uint64) color.RGBA {
	return color.RGBA{
		R: uint8(r >> 8),
		G: uint8(g >> 8),
		B: uint8(b >> 8),
		A: uint8(a >> 8),
	}
}
