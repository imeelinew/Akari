#!/usr/bin/env python3
"""Real PTY + AVFoundation smoke tests, with an isolated library and state.
Usage: python3 tests/tui_smoke.py [path/to/akari]
"""
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import wave
import fcntl

BINARY = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/akari').resolve())
NAMESPACE = hashlib.sha256(b'\n').hexdigest()


def run_case(keys, verify, terminate=False, size=(32, 100), track_seconds=30):
    with tempfile.TemporaryDirectory(prefix='akari-tui-test-') as directory:
        root = Path(directory)
        library = root / 'external'
        library.mkdir()
        tracks = {}
        filenames = {}
        for i, title in enumerate(['First Light', 'Second Wind', 'Third Moon'], 1):
            sid = f's{i}'
            name = f'{sid}.wav'
            with wave.open(str(library / name), 'wb') as audio:
                audio.setparams((1, 2, 8000, 0, 'NONE', 'not compressed'))
                audio.writeframes(b'\0\0' * 8000 * track_seconds)
            filenames[sid] = name
            tracks[sid] = dict(id=sid, title=title, artist='Test Artist', albumID='test-album', albumTitle='Test Album', duration=track_seconds, trackNumber=i)
        (library / 'manifest.json').write_text(json.dumps(dict(version=1, filenamesBySongID=filenames, songsBySongID=tracks)))
        config = root / 'config' / 'akari'
        config.mkdir(parents=True)
        (config / 'config.toml').write_text(f'[storage]\nmusic_dir="{root / "music"}"\nexternal_dirs=["{library}"]\n[player]\nspatial_audio=true\n')
        state_dir = root / 'cache' / 'akari' / NAMESPACE
        state_dir.mkdir(parents=True)
        # Null queue/recent are produced by the old Go version.
        (state_dir / 'playback.json').write_text(json.dumps(dict(queue=None, recent=None, index=-1, volume=0, position=0)))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', *size, 0, 0))
        before = termios.tcgetattr(slave)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'), TERM='xterm-256color')
        process = subprocess.Popen([BINARY], stdin=slave, stdout=slave, stderr=slave, env=env)
        output = bytearray()

        def drain(seconds):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                ready, _, _ = select.select([master], [], [], min(.05, max(0, until-time.monotonic())))
                if ready:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    output.extend(data)

        try:
            drain(1)
            assert process.poll() is None, output.decode(errors='replace')
            for text, delay in keys:
                os.write(master, text.encode())
                drain(delay)
            if terminate:
                process.send_signal(signal.SIGTERM)
            else:
                os.write(master, b'q')
            drain(.5)
            assert process.wait(timeout=8) == 0, output.decode(errors='replace')
            after = termios.tcgetattr(slave)
            assert before == after, 'terminal mode was not restored'
            state = json.loads((state_dir / 'playback.json').read_text())
            verify(state, output.decode(errors='replace'))
            assert all((library / name).exists() for name in filenames.values()), 'external audio was removed'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)


def queue_flow(state, output):
    assert [s['id'] for s in state['queue']] == ['s2'], state
    assert state['index'] == 0, state
    assert state['shuffle'] is True and state['repeat'] == 1, state
    assert state['volume'] == 5, state
    assert 'Playback failed' not in output, output
    assert 'PAUSED' in output and 'PLAYING' in output, 'audio never reached ready and pause states'


def search_flow(state, output):
    assert [s['id'] for s in state['queue']] == ['s3'], state
    assert state['index'] == 0, state
    assert 'Playback failed' not in output, output


def clear_flow(state, output):
    assert state['queue'] == [] and state['index'] == -1 and state['position'] == 0, state


run_case([('\r', .4), ('j', .1), ('\r', 1.5), (' ', .4), ('+osr', .3), ('3', .2), ('J', .2), ('x', .2), ('c', .2)], queue_flow)
print('PASS queue, reorder, remove, trim, pause, volume, shuffle, repeat, native audio')
run_case([('\r', .3), ('/Third\r', .2), ('\r', 1.2), ('?', .2), ('\x1b', .2)], search_flow, terminate=True)
print('PASS search, filtered playback, help, SIGTERM save, terminal restore')
run_case([('\r', .3), ('\r', 1), ('C', .3)], clear_flow, size=(16, 40))
print('PASS minimum terminal size, stop and clear')


def end_flow(state, output):
    assert len(state['queue']) == 3 and state['index'] == 2, state
    assert state['position'] == 0, state
    assert 'Playback failed' not in output, output


def repeat_one_flow(state, output):
    assert state['index'] == 0 and state['repeat'] == 2, state
    assert 0 <= state['position'] <= 1.1, state


run_case([('\r', .3), ('\r', 8)], end_flow, track_seconds=1)
print('PASS automatic next track and end-of-queue stop')
run_case([('\r', .3), ('rr\r', 3)], repeat_one_flow, track_seconds=1)
print('PASS repeat-one at native end of file')
