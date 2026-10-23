#!/usr/bin/env python3
"""End-to-end fake-Subsonic, Range, download, and offline restart smoke test.

Usage: python3 tests/remote_smoke.py [path/to/akari]
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit
import wave


ALBUM_ID = "remote-album"
SONG_ONE = "remote-song-one"
SONG_TWO = "remote-song-two"
ALBUM_TITLE = "Remote Album"
SONG_ONE_TITLE = "First Remote Song"
SONG_TWO_TITLE = "Second Remote Song"
USERNAME = "smoke-user"
PASSWORD = "smoke-password"
TRACK_SECONDS = 8
ANSI_ESCAPE = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")


def silent_wav(seconds: int = TRACK_SECONDS) -> bytes:
    output = io.BytesIO()
    with wave.open(output, "wb") as audio:
        audio.setparams((1, 2, 8_000, 0, "NONE", "not compressed"))
        audio.writeframes(b"\0\0" * 8_000 * seconds)
    return output.getvalue()


class FakeSubsonic:
    def __init__(self) -> None:
        self.audio = silent_wav()
        self.requests: list[dict[str, object]] = []
        self.errors: list[str] = []
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:
                owner.handle(self, head=False)

            def do_HEAD(self) -> None:
                owner.handle(self, head=True)

            def log_message(self, _format: str, *_args: object) -> None:
                return

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(
            target=self.server.serve_forever,
            kwargs={"poll_interval": 0.05},
            name="fake-subsonic",
            daemon=True,
        )
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/subsonic"

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

    def snapshot(self) -> list[dict[str, object]]:
        with self.lock:
            return [dict(item) for item in self.requests]

    def record(self, record: dict[str, object]) -> None:
        with self.lock:
            self.requests.append(record)

    def record_error(self, error: BaseException) -> None:
        with self.lock:
            self.errors.append(f"{type(error).__name__}: {error}")

    def handle(self, request: BaseHTTPRequestHandler, *, head: bool) -> None:
        split = urlsplit(request.path)
        query = {key: values[-1] for key, values in parse_qs(split.query).items()}
        record: dict[str, object] = {
            "method": request.command,
            "path": split.path,
            "query": query,
            "range": request.headers.get("Range"),
        }
        self.record(record)
        try:
            prefix = "/subsonic/rest/"
            if not split.path.startswith(prefix) or not split.path.endswith(".view"):
                self.respond(request, 404, b"not found", "text/plain", head)
                return
            action = split.path[len(prefix) : -len(".view")]
            if not self.authorized(query):
                self.respond(request, 401, b"unauthorized", "text/plain", head)
                return
            if action == "getAlbumList2":
                self.send_json(
                    request,
                    {
                        "subsonic-response": {
                            "status": "ok",
                            "version": "1.16.1",
                            "albumList2": {
                                "album": [
                                    {
                                        "id": ALBUM_ID,
                                        "name": ALBUM_TITLE,
                                        "artist": "Remote Artist",
                                        "genre": "Test",
                                        "year": 2026,
                                        "played": {
                                            "date": "2026-10-23T12:00:00.123456789Z"
                                        },
                                    }
                                ]
                            },
                        }
                    },
                    head,
                )
            elif action == "getAlbum":
                if query.get("id") != ALBUM_ID:
                    self.send_json(
                        request,
                        {
                            "subsonic-response": {
                                "status": "failed",
                                "error": {"code": 70, "message": "unknown album"},
                            }
                        },
                        head,
                    )
                    return
                self.send_json(
                    request,
                    {
                        "subsonic-response": {
                            "status": "ok",
                            "version": "1.16.1",
                            "album": {
                                "name": ALBUM_TITLE,
                                "coverArt": "cover",
                                "song": [
                                    {
                                        "id": SONG_ONE,
                                        "title": SONG_ONE_TITLE,
                                        "artist": "Remote Artist",
                                        "album": ALBUM_TITLE,
                                        "duration": TRACK_SECONDS,
                                        "track": 1,
                                        "discNumber": 1,
                                    },
                                    {
                                        "id": SONG_TWO,
                                        "title": SONG_TWO_TITLE,
                                        "artist": "Remote Artist",
                                        "album": ALBUM_TITLE,
                                        "duration": TRACK_SECONDS,
                                        "track": 2,
                                        "discNumber": 1,
                                    },
                                ],
                            },
                        }
                    },
                    head,
                )
            elif action in {"stream", "download"}:
                if query.get("format") != "raw":
                    self.send_json(
                        request,
                        {
                            "subsonic-response": {
                                "status": "failed",
                                "error": {"code": 10, "message": "raw required"},
                            }
                        },
                        head,
                    )
                    return
                self.send_audio(request, head)
            else:
                self.send_json(
                    request,
                    {
                        "subsonic-response": {
                            "status": "failed",
                            "error": {"code": 0, "message": f"unknown action {action}"},
                        }
                    },
                    head,
                )
        except (BrokenPipeError, ConnectionResetError):
            pass
        except BaseException as error:
            self.record_error(error)
            try:
                self.respond(request, 500, b"server error", "text/plain", head)
            except OSError:
                pass

    def authorized(self, query: dict[str, str]) -> bool:
        salt = query.get("s", "")
        expected = hashlib.md5((PASSWORD + salt).encode()).hexdigest()
        return (
            query.get("u") == USERNAME
            and len(salt) == 32
            and query.get("t") == expected
            and query.get("v") == "1.16.1"
            and query.get("c") == "Akari"
        )

    def send_json(
        self,
        request: BaseHTTPRequestHandler,
        value: dict[str, object],
        head: bool,
    ) -> None:
        self.respond(
            request,
            200,
            json.dumps(value, separators=(",", ":")).encode(),
            "application/json",
            head,
        )

    def respond(
        self,
        request: BaseHTTPRequestHandler,
        status: int,
        body: bytes,
        content_type: str,
        head: bool,
        extra_headers: dict[str, str] | None = None,
    ) -> None:
        request.send_response(status)
        request.send_header("Content-Type", content_type)
        request.send_header("Content-Length", str(len(body)))
        for key, value in (extra_headers or {}).items():
            request.send_header(key, value)
        request.end_headers()
        if not head:
            request.wfile.write(body)

    def send_audio(self, request: BaseHTTPRequestHandler, head: bool) -> None:
        total = len(self.audio)
        range_header = request.headers.get("Range")
        if range_header is None:
            self.respond(
                request,
                200,
                self.audio,
                "audio/wav",
                head,
                {
                    "Accept-Ranges": "bytes",
                    "Content-Disposition": 'attachment; filename="remote.wav"',
                },
            )
            return
        parsed = parse_range(range_header, total)
        if parsed is None:
            self.respond(
                request,
                416,
                b"",
                "audio/wav",
                head,
                {"Content-Range": f"bytes */{total}", "Accept-Ranges": "bytes"},
            )
            return
        start, end = parsed
        body = self.audio[start : end + 1]
        self.respond(
            request,
            206,
            body,
            "audio/wav",
            head,
            {
                "Accept-Ranges": "bytes",
                "Content-Range": f"bytes {start}-{end}/{total}",
                "Content-Disposition": 'attachment; filename="remote.wav"',
            },
        )


def parse_range(value: str, total: int) -> tuple[int, int] | None:
    if not value.startswith("bytes=") or "," in value:
        return None
    span = value[len("bytes=") :].strip()
    if "-" not in span:
        return None
    first, last = span.split("-", 1)
    try:
        if first == "":
            suffix = int(last)
            if suffix <= 0:
                return None
            start = max(0, total - suffix)
            return start, total - 1
        start = int(first)
        if start < 0 or start >= total:
            return None
        end = total - 1 if last == "" else min(int(last), total - 1)
        if end < start:
            return None
        return start, end
    except ValueError:
        return None


class AkariSession:
    def __init__(self, binary: Path, env: dict[str, str], size: tuple[int, int] = (32, 100)):
        self.binary = binary
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", *size, 0, 0))
        self.terminal_before = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.process = subprocess.Popen(
            [str(binary)],
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            env=env,
            close_fds=True,
        )

    def text(self) -> str:
        return ANSI_ESCAPE.sub(b"", bytes(self.output)).decode(errors="replace")

    def mark(self) -> int:
        return len(self.output)

    def drain(self, seconds: float) -> None:
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            timeout = min(0.05, max(0.0, until - time.monotonic()))
            ready, _, _ = select.select([self.master], [], [], timeout)
            if not ready:
                continue
            try:
                data = os.read(self.master, 65_536)
            except OSError:
                break
            if not data:
                break
            self.output.extend(data)

    def wait_text(self, text: str, timeout: float = 8.0, since: int = 0) -> None:
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            self.drain(0.05)
            segment = ANSI_ESCAPE.sub(
                b"", bytes(self.output[since:])
            ).decode(errors="replace")
            if text in segment or re.sub(r"\s+", "", text) in re.sub(
                r"\s+", "", segment
            ):
                return
            if self.process.poll() is not None:
                raise AssertionError(
                    f"Akari exited before rendering {text!r}\n{self.text()}"
                )
        raise AssertionError(f"timed out waiting for {text!r}\n{self.text()}")

    def send(self, value: str, settle: float = 0.15) -> None:
        os.write(self.master, value.encode())
        self.drain(settle)

    def quit(self) -> None:
        if self.process.poll() is None:
            self.send("q", 0.2)
        try:
            code = self.process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.process.send_signal(signal.SIGTERM)
            try:
                code = self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                code = self.process.wait(timeout=3)
        self.drain(0.1)
        if code != 0:
            raise AssertionError(f"Akari exited with {code}\n{self.text()}")
        after = termios.tcgetattr(self.slave)
        if after != self.terminal_before:
            raise AssertionError("terminal mode was not restored")

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=3)
        os.close(self.master)
        os.close(self.slave)


def wait_until(predicate, timeout: float, description: str):
    deadline = time.monotonic() + timeout
    last_error: BaseException | None = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except BaseException as error:
            last_error = error
        time.sleep(0.05)
    if last_error is not None:
        raise AssertionError(f"timed out waiting for {description}: {last_error}") from last_error
    raise AssertionError(f"timed out waiting for {description}")


def config_text(root: Path, server_url: str) -> str:
    music_dir = json.dumps(str(root / "music"))
    return (
        "[server]\n"
        f"url = {json.dumps(server_url)}\n"
        f"username = {json.dumps(USERNAME)}\n"
        f"password = {json.dumps(PASSWORD)}\n\n"
        "[storage]\n"
        f"music_dir = {music_dir}\n"
        "external_dirs = []\n\n"
        "[player]\n"
        "spatial_audio = false\n"
    )


def app_env(root: Path) -> dict[str, str]:
    home = root / "home"
    config = root / "config"
    cache = root / "cache"
    for directory in (home, config / "akari", cache):
        directory.mkdir(parents=True, exist_ok=True)
    return dict(
        os.environ,
        HOME=str(home),
        XDG_CONFIG_HOME=str(config),
        XDG_CACHE_HOME=str(cache),
        TERM="xterm-256color",
    )


def action_requests(
    server: FakeSubsonic, action: str
) -> list[dict[str, object]]:
    return [
        request
        for request in server.snapshot()
        if request["path"] == f"/subsonic/rest/{action}.view"
    ]


def load_json(path: Path) -> dict[str, object]:
    return json.loads(path.read_text())


def write_failure_evidence(
    root: Path,
    server: FakeSubsonic,
    sessions: list[AkariSession],
    error: BaseException,
) -> Path:
    evidence = root / "failure-evidence.txt"
    lines = [
        f"error: {type(error).__name__}: {error}",
        "",
        "requests:",
        json.dumps(server.snapshot(), indent=2),
        "",
        f"server errors: {server.errors!r}",
        "",
    ]
    for index, session in enumerate(sessions, 1):
        lines.extend(
            [
                f"--- session {index} ---",
                session.text(),
                "",
            ]
        )
    lines.extend(["--- traceback ---", traceback.format_exc()])
    evidence.write_text("\n".join(lines))
    return evidence


def run(binary: Path) -> None:
    if not binary.is_file():
        raise SystemExit(f"Akari binary not found: {binary}")
    root = Path(tempfile.mkdtemp(prefix="akari-remote-smoke-"))
    server = FakeSubsonic()
    sessions: list[AkariSession] = []
    succeeded = False
    try:
        env = app_env(root)
        config = root / "config" / "akari" / "config.toml"
        config.write_text(config_text(root, server.url))
        namespace = hashlib.sha256(f"{server.url}\n{USERNAME}".encode()).hexdigest()
        music_namespace = root / "music" / namespace
        cache_namespace = root / "cache" / "akari" / namespace
        index_path = music_namespace / "index.json"

        print(f"artifact root: {root}")
        print(f"fake Subsonic: {server.url}")
        print("phase 1: remote albums, songs, Range stream, download, index")
        first = AkariSession(binary, env)
        sessions.append(first)
        first.wait_text(ALBUM_TITLE)
        first.send("\r")
        first.wait_text(SONG_ONE_TITLE)
        first.wait_text(SONG_TWO_TITLE)
        first.send("\r", 0.5)

        stream_requests = wait_until(
            lambda: [
                request
                for request in action_requests(server, "stream")
                if request["range"] and request["query"].get("format") == "raw"
            ],
            10,
            "a raw Subsonic stream request with an HTTP Range",
        )
        first.wait_text("PLAYING", timeout=10)
        first.send("j")
        first.send("d")

        download_requests = wait_until(
            lambda: [
                request
                for request in action_requests(server, "download")
                if request["range"] and request["query"].get("format") == "raw"
            ],
            10,
            "a raw Subsonic download request with an HTTP Range",
        )
        index = wait_until(
            lambda: load_json(index_path) if index_path.is_file() else None,
            10,
            "download index",
        )
        records = index.get("songsBySongID") or {}
        assert index.get("version") == 1, index
        assert SONG_TWO in records, index
        record = records[SONG_TWO]
        assert record["song"]["id"] == SONG_TWO, record
        assert record["song"]["albumID"] == ALBUM_ID, record
        local_file = music_namespace / "audio" / record["filename"]
        assert local_file.is_file(), local_file
        assert local_file.suffix == ".wav", local_file
        assert local_file.read_bytes() == server.audio, local_file

        downloaded_mark = first.mark()
        first.send("2", 0.3)
        first.wait_text("Downloaded", since=downloaded_mark)
        first.wait_text(ALBUM_TITLE, since=downloaded_mark)
        songs_mark = first.mark()
        first.send("\r", 0.3)
        first.wait_text(SONG_TWO_TITLE, since=songs_mark)
        first.wait_text("Local", since=songs_mark)
        first.quit()

        assert all(item["query"].get("id") == SONG_ONE for item in stream_requests), stream_requests
        assert all(item["query"].get("id") == SONG_TWO for item in download_requests), download_requests
        assert action_requests(server, "getAlbumList2"), server.snapshot()
        assert action_requests(server, "getAlbum"), server.snapshot()
        assert not server.errors, server.errors
        print(
            "PASS albums -> songs -> raw Range stream -> d download -> "
            "index/file -> Downloaded"
        )

        print("phase 2: stop server and restart binary with cached metadata/local file")
        request_count = len(server.snapshot())
        server.stop()
        second = AkariSession(binary, env)
        sessions.append(second)
        second.wait_text(ALBUM_TITLE, timeout=8)
        offline_albums_mark = second.mark()
        second.send("2", 0.3)
        second.wait_text("Downloaded", since=offline_albums_mark)
        offline_songs_mark = second.mark()
        second.send("\r", 0.3)
        second.wait_text(SONG_TWO_TITLE, since=offline_songs_mark)
        second.send("j")
        offline_play_mark = second.mark()
        second.send("\r", 0.5)
        second.wait_text("PLAYING", timeout=10, since=offline_play_mark)
        assert "Playback failed" not in second.text(), second.text()
        second.quit()

        assert len(server.snapshot()) == request_count, server.snapshot()
        playback_path = cache_namespace / "playback.json"
        playback = load_json(playback_path)
        assert [song["id"] for song in playback["queue"]] == [SONG_ONE, SONG_TWO], playback
        assert playback["index"] == 1, playback
        print("PASS disconnected server -> cached albums/songs -> local playback, no server I/O")

        succeeded = True
        print("PASS remote library end-to-end smoke")
    except BaseException as error:
        evidence = write_failure_evidence(root, server, sessions, error)
        print(f"\nFAIL: {type(error).__name__}: {error}", file=sys.stderr)
        print(f"evidence: {evidence}", file=sys.stderr)
        print(f"artifacts kept: {root}", file=sys.stderr)
        raise
    finally:
        for session in sessions:
            try:
                session.close()
            except OSError:
                pass
        if server.thread.is_alive():
            try:
                server.stop()
            except RuntimeError:
                pass
        if succeeded:
            shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "binary",
        nargs="?",
        default="target/debug/akari",
        help="path to the Akari binary",
    )
    arguments = parser.parse_args()
    run(Path(arguments.binary).resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
