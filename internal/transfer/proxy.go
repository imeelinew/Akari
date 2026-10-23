package transfer

import (
	"context"
	"errors"
	"fmt"
	"io"
	"mime"
	"net/http"
	"os"
	"path"
	"strconv"
	"strings"
	"time"
)

const (
	defaultReadHeaderTimeout = 5 * time.Second
	playbackChunkSize        = int64(1 << 20)
	manualChunkSize          = int64(4 << 20)
)

var errRangeNotSatisfiable = errors.New("transfer: range not satisfiable")
var errPlaybackClientDisconnected = errors.New("transfer: playback client disconnected")

type clientRange struct {
	hasRange bool
	start    int64
	end      int64
	suffix   int64
}

type rangeResult struct {
	start       int64
	end         int64
	total       int64
	contentType string
	extension   string
	disposition string
}

func (m *Manager) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	if req.URL.Path != "/stream" {
		http.NotFound(w, req)
		return
	}
	if req.Method != http.MethodGet && req.Method != http.MethodHead {
		w.Header().Set("Allow", "GET, HEAD")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	songID := req.URL.Query().Get("id")
	token := req.URL.Query().Get("token")
	m.mu.Lock()
	e := m.entries[songID]
	if m.closed || e == nil {
		m.mu.Unlock()
		http.NotFound(w, req)
		return
	}
	e.mu.Lock()
	run := e.playback
	if run == nil || run.token != token {
		e.mu.Unlock()
		m.mu.Unlock()
		http.NotFound(w, req)
		return
	}
	run.handlers++
	e.activeHandlers++
	e.discardRequested = false
	m.handlerWG.Add(1)
	e.mu.Unlock()
	m.mu.Unlock()

	ctx, cancel := context.WithCancel(req.Context())
	stop := context.AfterFunc(run.ctx, cancel)
	defer func() {
		stop()
		cancel()
		m.finishHandler(e, run)
		m.handlerWG.Done()
	}()

	tracked := &trackingResponseWriter{ResponseWriter: w}
	if err := m.serveSong(ctx, tracked, req, e, run.sourceURL); err != nil {
		if errors.Is(err, context.Canceled) || errors.Is(err, errRangeNotSatisfiable) ||
			errors.Is(err, errPlaybackClientDisconnected) {
			return
		}
		m.recordTransferError(e, err)
		if !tracked.started {
			http.Error(tracked, "source transfer failed", http.StatusBadGateway)
		}
	}
}

func (m *Manager) serveSong(
	ctx context.Context,
	w http.ResponseWriter,
	req *http.Request,
	e *entry,
	sourceURL string,
) error {
	requested, err := parseClientRange(req.Header.Get("Range"))
	if err != nil {
		w.Header().Set("Content-Range", "bytes */*")
		http.Error(w, "invalid byte range", http.StatusRequestedRangeNotSatisfiable)
		return nil
	}

	if requested.hasRange && requested.suffix == 0 && requested.start < 0 {
		return m.writeRangeError(w, e)
	}

	e.mu.Lock()
	total := e.total
	start := requested.start
	end := requested.end
	suffix := requested.suffix
	e.mu.Unlock()

	if requested.hasRange && suffix > 0 && total < 0 {
		if _, _, err := m.fillChunk(ctx, e, sourceURL, 0, playbackChunkSize-1); err != nil {
			return err
		}
		e.mu.Lock()
		total = e.total
		e.mu.Unlock()
	}

	needProbe := total < 0
	if requested.hasRange && suffix == 0 {
		needProbe = needProbe || !containsRange(e.rangesSnapshot(), start, start)
	}
	if needProbe {
		probeStart := start
		if !requested.hasRange {
			probeStart = 0
		} else if suffix > 0 {
			probeStart = 0
		}
		if probeStart < 0 {
			probeStart = 0
		}
		probeEnd := probeStart + playbackChunkSize - 1
		if requested.hasRange && suffix == 0 && end >= 0 && probeEnd > end {
			probeEnd = end
		}
		if _, _, err := m.fillChunk(ctx, e, sourceURL, probeStart, probeEnd); err != nil {
			if errors.Is(err, errRangeNotSatisfiable) {
				return m.writeRangeError(w, e)
			}
			return err
		}
		e.mu.Lock()
		total = e.total
		e.mu.Unlock()
	}

	if requested.hasRange {
		if suffix > 0 {
			if total <= 0 {
				return m.writeRangeError(w, e)
			}
			start = total - suffix
			if start < 0 {
				start = 0
			}
			end = total - 1
		}
		if start < 0 || (total >= 0 && start >= total) {
			return m.writeRangeError(w, e)
		}
		if total >= 0 && (end < 0 || end >= total) {
			end = total - 1
		}
		if end >= 0 && end < start {
			return m.writeRangeError(w, e)
		}
		writeHeaders(w, e, true, start, end, total)
	} else {
		start = 0
		end = -1
		if total >= 0 {
			end = total - 1
		}
		writeHeaders(w, e, false, start, end, total)
	}

	if req.Method == http.MethodHead {
		w.WriteHeader(statusForRange(requested.hasRange))
		return nil
	}
	w.WriteHeader(statusForRange(requested.hasRange))

	for end < 0 || start <= end {
		chunkStart := start
		chunkEnd := start + playbackChunkSize - 1
		if end >= 0 && chunkEnd > end {
			chunkEnd = end
		}

		for {
			ranges := e.rangesSnapshot()
			holeStart, holeEnd, missing := missingChunk(ranges, start, chunkEnd)
			if !missing {
				break
			}
			result, count, err := m.fillChunk(ctx, e, sourceURL, holeStart, holeEnd)
			if err != nil {
				return err
			}
			if count == 0 || result.end < holeStart {
				if result.total >= 0 && holeStart >= result.total {
					return nil
				}
				e.mu.Lock()
				if e.total < 0 {
					e.total = holeStart
				}
				e.mu.Unlock()
				if result.total < 0 {
					return nil
				}
				return errors.New("source made no progress")
			}
			if result.end < start {
				return errRangeNotSatisfiable
			}
			if result.total >= 0 && chunkEnd >= result.total {
				chunkEnd = result.total - 1
				if end < 0 {
					end = result.total - 1
				}
			}
		}

		next, err := copyCached(ctx, w, e.cachePath, start, chunkEnd)
		if err != nil {
			return err
		}
		if next <= chunkStart {
			return nil
		}
		delivered := next - chunkStart
		start = next
		if end < 0 {
			e.mu.Lock()
			total = e.total
			e.mu.Unlock()
			if total >= 0 {
				end = total - 1
				if start > end {
					return nil
				}
			} else if delivered < playbackChunkSize {
				// A short source response without a total marks EOF.
				e.mu.Lock()
				e.total = next
				total = next
				e.mu.Unlock()
				return nil
			}
		}
	}
	return nil
}

func (m *Manager) downloadAll(ctx context.Context, e *entry, sourceURL string) error {
	var start int64
	for {
		e.mu.Lock()
		total := e.total
		ranges := append([]byteRange(nil), e.ranges...)
		e.mu.Unlock()

		if total >= 0 && start >= total {
			return nil
		}
		end := start + manualChunkSize - 1
		if total >= 0 && end >= total {
			end = total - 1
		}
		if holeStart, holeEnd, missing := missingChunk(ranges, start, end); missing {
			result, count, err := m.fillChunk(ctx, e, sourceURL, holeStart, holeEnd)
			if errors.Is(err, errRangeNotSatisfiable) {
				e.mu.Lock()
				newTotal := e.total
				e.mu.Unlock()
				if newTotal >= 0 && holeStart >= newTotal {
					return nil
				}
				return err
			}
			if err != nil {
				return err
			}
			if count == 0 || result.end < holeStart {
				if result.total >= 0 {
					start = result.total
					continue
				}
				e.mu.Lock()
				e.total = holeStart
				e.mu.Unlock()
				return nil
			}
			start = result.end + 1
			if result.total >= 0 && start >= result.total {
				return nil
			}
			continue
		}

		if total < 0 {
			// A cache without a known total cannot happen after a successful fetch.
			return errors.New("source length is unavailable")
		}
		if start >= end && total >= 0 {
			return nil
		}
		start = end + 1
	}
}

func (m *Manager) fillChunk(
	ctx context.Context,
	e *entry,
	sourceURL string,
	start int64,
	requestedEnd int64,
) (rangeResult, int64, error) {
	select {
	case e.fetch <- struct{}{}:
	case <-ctx.Done():
		return rangeResult{}, 0, ctx.Err()
	}
	defer func() { <-e.fetch }()

	if requestedEnd < start {
		requestedEnd = start
	}

	e.mu.Lock()
	total := e.total
	e.mu.Unlock()
	if total >= 0 && start >= total {
		return rangeResult{start: start, end: start - 1, total: total}, 0, errRangeNotSatisfiable
	}
	if total >= 0 && requestedEnd >= total {
		requestedEnd = total - 1
	}
	e.mu.Lock()
	if containsRange(e.ranges, start, requestedEnd) {
		result := rangeResult{start: start, end: requestedEnd, total: e.total, contentType: e.contentType, extension: e.extension, disposition: e.disposition}
		e.mu.Unlock()
		return result, requestedEnd - start + 1, nil
	}
	e.mu.Unlock()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, sourceURL, nil)
	if err != nil {
		return rangeResult{}, 0, errors.New("invalid source request")
	}
	req.Header.Set("Range", fmt.Sprintf("bytes=%d-%d", start, requestedEnd))

	resp, err := m.client.Do(req)
	if err != nil {
		if ctx.Err() != nil {
			return rangeResult{}, 0, ctx.Err()
		}
		return rangeResult{}, 0, errors.New("source request failed")
	}
	defer resp.Body.Close()

	contentType, err := validateResponseContentType(resp.Header.Get("Content-Type"))
	if err != nil {
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4<<10))
		return rangeResult{}, 0, err
	}

	result := rangeResult{
		start:       start,
		total:       -1,
		contentType: contentType,
		extension:   extensionFromURL(sourceURL),
		disposition: resp.Header.Get("Content-Disposition"),
	}

	var readStart int64
	var maxLength int64
	switch resp.StatusCode {
	case http.StatusPartialContent:
		rangeStart, rangeEnd, rangeTotal, parseErr := parseContentRange(resp.Header.Get("Content-Range"))
		if parseErr != nil || rangeStart != start || rangeEnd < rangeStart {
			return rangeResult{}, 0, errors.New("source returned an invalid byte range")
		}
		result.start = rangeStart
		result.end = rangeEnd
		result.total = rangeTotal
		readStart = rangeStart
		maxLength = rangeEnd - rangeStart + 1
		if limit := requestedEnd - start + 1; maxLength > limit {
			maxLength = limit
			result.end = start + limit - 1
		}
	case http.StatusOK:
		readStart = 0
		if resp.ContentLength >= 0 {
			result.total = resp.ContentLength
		}
		maxLength = requestedEnd - start + 1
		if result.total >= 0 && start+maxLength > result.total {
			maxLength = result.total - start
		}
		if resp.ContentLength >= 0 && start == 0 {
			result.end = result.total - 1
		}
	case http.StatusRequestedRangeNotSatisfiable:
		_, _, rangeTotal, parseErr := parseContentRange(resp.Header.Get("Content-Range"))
		if parseErr == nil && rangeTotal >= 0 {
			e.mu.Lock()
			if e.total < 0 {
				e.total = rangeTotal
			}
			e.mu.Unlock()
			result.total = rangeTotal
		}
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4<<10))
		return result, 0, errRangeNotSatisfiable
	default:
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4<<10))
		return rangeResult{}, 0, fmt.Errorf("source returned HTTP %d", resp.StatusCode)
	}

	if maxLength < 0 {
		maxLength = 0
	}
	if readStart < start {
		skip := start - readStart
		if skip > 0 {
			if _, err := io.CopyN(io.Discard, resp.Body, skip); err != nil {
				if ctx.Err() != nil {
					return result, 0, ctx.Err()
				}
				return result, 0, errors.New("source stream interrupted")
			}
			readStart = start
		}
	}

	file, err := os.OpenFile(e.cachePath, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return result, 0, errors.New("open playback cache failed")
	}
	defer file.Close()

	if result.total >= 0 {
		if err := file.Truncate(result.total); err != nil {
			return result, 0, errors.New("resize playback cache failed")
		}
	}

	writer := &cacheWriter{
		file:   file,
		offset: readStart,
	}
	_, copyErr := io.CopyBuffer(writer, io.LimitReader(resp.Body, maxLength), make([]byte, 32<<10))
	if copyErr == nil && result.total < 0 && writer.written < maxLength {
		result.total = start + writer.written
	}
	e.mu.Lock()
	if writer.written > 0 {
		end := writer.offset - 1
		e.ranges = addCoverage(e.ranges, start, end)
	}
	if result.total >= 0 {
		if e.total >= 0 && e.total != result.total {
			copyErr = errors.New("source length changed")
		} else {
			e.total = result.total
		}
	}
	if e.contentType == "" && result.contentType != "" {
		e.contentType = result.contentType
	}
	if e.extension == "" && result.extension != "" {
		e.extension = result.extension
	}
	if e.disposition == "" && result.disposition != "" {
		e.disposition = result.disposition
	}
	if writer.written > 0 {
		e.lastErr = ""
	}
	e.mu.Unlock()

	result.end = writer.offset - 1
	if writer.written == 0 {
		result.end = start - 1
	}
	if result.total >= 0 && result.end == result.total-1 {
		if err := file.Sync(); err != nil {
			return result, writer.written, errors.New("sync playback cache failed")
		}
	}

	if copyErr != nil {
		if ctx.Err() != nil {
			return result, writer.written, ctx.Err()
		}
		return result, writer.written, errors.New("source stream interrupted")
	}
	return result, writer.written, nil
}

type cacheWriter struct {
	file    *os.File
	offset  int64
	written int64
}

func (w *cacheWriter) Write(data []byte) (int, error) {
	written, err := w.file.WriteAt(data, w.offset)
	w.offset += int64(written)
	w.written += int64(written)
	return written, err
}

func parseClientRange(header string) (clientRange, error) {
	header = strings.TrimSpace(header)
	if header == "" {
		return clientRange{}, nil
	}
	if strings.Contains(header, ",") {
		return clientRange{}, errors.New("multiple ranges")
	}
	unit, spec, ok := strings.Cut(header, "=")
	if !ok || !strings.EqualFold(strings.TrimSpace(unit), "bytes") {
		return clientRange{}, errors.New("invalid range unit")
	}
	spec = strings.TrimSpace(spec)
	first, last, ok := strings.Cut(spec, "-")
	if !ok {
		return clientRange{}, errors.New("invalid range")
	}
	first = strings.TrimSpace(first)
	last = strings.TrimSpace(last)
	if first == "" {
		suffix, err := strconv.ParseInt(last, 10, 64)
		if err != nil || suffix <= 0 {
			return clientRange{}, errors.New("invalid suffix range")
		}
		return clientRange{hasRange: true, start: -1, end: -1, suffix: suffix}, nil
	}

	start, err := strconv.ParseInt(first, 10, 64)
	if err != nil || start < 0 {
		return clientRange{}, errors.New("invalid range start")
	}
	end := int64(-1)
	if last != "" {
		end, err = strconv.ParseInt(last, 10, 64)
		if err != nil || end < start {
			return clientRange{}, errors.New("invalid range end")
		}
	}
	return clientRange{hasRange: true, start: start, end: end}, nil
}

func parseContentRange(header string) (int64, int64, int64, error) {
	unit, value, ok := strings.Cut(strings.TrimSpace(header), " ")
	if !ok || !strings.EqualFold(unit, "bytes") {
		return 0, 0, 0, errors.New("invalid content range")
	}
	value = strings.TrimSpace(value)
	span, totalText, ok := strings.Cut(value, "/")
	if !ok {
		return 0, 0, 0, errors.New("invalid content range")
	}
	span = strings.TrimSpace(span)
	totalText = strings.TrimSpace(totalText)
	total := int64(-1)
	var err error
	if totalText != "*" {
		total, err = strconv.ParseInt(totalText, 10, 64)
		if err != nil || total < 0 {
			return 0, 0, 0, errors.New("invalid content range total")
		}
	}
	if span == "*" {
		return 0, 0, total, nil
	}
	startText, endText, ok := strings.Cut(span, "-")
	if !ok {
		return 0, 0, 0, errors.New("invalid content range span")
	}
	startText = strings.TrimSpace(startText)
	endText = strings.TrimSpace(endText)
	start, err := strconv.ParseInt(startText, 10, 64)
	if err != nil || start < 0 {
		return 0, 0, 0, errors.New("invalid content range start")
	}
	end, err := strconv.ParseInt(endText, 10, 64)
	if err != nil || end < start {
		return 0, 0, 0, errors.New("invalid content range end")
	}
	if total >= 0 && end >= total {
		return 0, 0, 0, errors.New("content range exceeds total")
	}
	return start, end, total, nil
}

func validateResponseContentType(header string) (string, error) {
	header = strings.TrimSpace(header)
	if header == "" {
		return "", nil
	}
	mediaType, _, err := mime.ParseMediaType(header)
	if err != nil {
		return "", errors.New("source returned an invalid content type")
	}
	mediaType = strings.ToLower(mediaType)
	if mediaType == "application/json" || mediaType == "application/xml" ||
		mediaType == "text/xml" || strings.HasSuffix(mediaType, "+json") ||
		strings.HasSuffix(mediaType, "+xml") {
		return "", errors.New("source returned a JSON or XML error")
	}
	return mediaType, nil
}

func writeHeaders(w http.ResponseWriter, e *entry, ranged bool, start, end, total int64) {
	e.mu.Lock()
	contentType := e.contentType
	e.mu.Unlock()
	if contentType == "" {
		contentType = "application/octet-stream"
	}
	w.Header().Set("Accept-Ranges", "bytes")
	w.Header().Set("Content-Type", contentType)
	if !ranged && total >= 0 {
		w.Header().Set("Content-Length", strconv.FormatInt(total, 10))
		return
	}
	if ranged {
		if total >= 0 {
			w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", start, end, total))
			w.Header().Set("Content-Length", strconv.FormatInt(end-start+1, 10))
		} else if end >= start {
			w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/*", start, end))
			w.Header().Set("Content-Length", strconv.FormatInt(end-start+1, 10))
		}
	}
}

func (m *Manager) writeRangeError(w http.ResponseWriter, e *entry) error {
	e.mu.Lock()
	total := e.total
	e.mu.Unlock()
	if total >= 0 {
		w.Header().Set("Content-Range", fmt.Sprintf("bytes */%d", total))
	} else {
		w.Header().Set("Content-Range", "bytes */*")
	}
	http.Error(w, "range not satisfiable", http.StatusRequestedRangeNotSatisfiable)
	return errRangeNotSatisfiable
}

func copyCached(ctx context.Context, dst io.Writer, cachePath string, start, end int64) (int64, error) {
	file, err := os.Open(cachePath)
	if err != nil {
		return start, errors.New("open playback cache failed")
	}
	defer file.Close()

	reader := io.NewSectionReader(file, start, end-start+1)
	buffer := make([]byte, 32<<10)
	position := start
	for position <= end {
		if err := ctx.Err(); err != nil {
			return position, err
		}
		limit := int64(len(buffer))
		if remaining := end - position + 1; remaining < limit {
			limit = remaining
		}
		read, readErr := reader.Read(buffer[:limit])
		if read > 0 {
			written, writeErr := dst.Write(buffer[:read])
			position += int64(written)
			if writeErr != nil {
				return position, errPlaybackClientDisconnected
			}
			if written != read {
				return position, errPlaybackClientDisconnected
			}
		}
		if readErr != nil {
			if errors.Is(readErr, io.EOF) {
				return position, nil
			}
			return position, errors.New("read playback cache failed")
		}
	}
	return position, nil
}

func statusForRange(ranged bool) int {
	if ranged {
		return http.StatusPartialContent
	}
	return http.StatusOK
}

type trackingResponseWriter struct {
	http.ResponseWriter
	started bool
}

func (w *trackingResponseWriter) WriteHeader(status int) {
	w.started = true
	w.ResponseWriter.WriteHeader(status)
}

func (w *trackingResponseWriter) Write(data []byte) (int, error) {
	w.started = true
	return w.ResponseWriter.Write(data)
}

func (e *entry) rangesSnapshot() []byteRange {
	e.mu.Lock()
	defer e.mu.Unlock()
	return append([]byteRange(nil), e.ranges...)
}

func dispositionExtension(header string) string {
	if header == "" {
		return ""
	}
	_, params, err := mime.ParseMediaType(header)
	if err != nil {
		return ""
	}
	filename := params["filename"]
	if filename == "" {
		return ""
	}
	return supportedAudioExtension(path.Ext(filename))
}
