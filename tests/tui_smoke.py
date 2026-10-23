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


def cover_and_spectrum_flow(program='kitty'):
    """Inspect actual PTY bytes: scrolling/FFT must never replay Kitty artwork."""
    import math
    import re
    import zlib

    def png(rgb):
        def chunk(kind, data):
            return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
        return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', 64, 64, 8, 2, 0, 0, 0))
                + chunk(b'IDAT', zlib.compress((b'\0' + bytes(rgb) * 64) * 64)) + chunk(b'IEND', b''))

    with tempfile.TemporaryDirectory(prefix='akari-cover-test-') as directory:
        root = Path(directory)
        library = root / 'external'
        library.mkdir()
        with wave.open(str(library / 'tone.wav'), 'wb') as audio:
            audio.setparams((1, 2, 44100, 0, 'NONE', 'not compressed'))
            cycle = b''.join(struct.pack('<h', int(16000 * math.sin(i * 2 * math.pi * 440 / 44100))) for i in range(44100))
            audio.writeframes(cycle * 30)
        (library / 'red.png').write_bytes(png([255, 0, 0]))
        (library / 'blue.png').write_bytes(png([0, 0, 255]))
        songs = {f's{i:02}': dict(id=f's{i:02}', title=f'Track {i:02}', artist='Test Artist',
                  albumID=f'a{i:02}', albumTitle=f'Album {i:02}', duration=30, trackNumber=1) for i in range(40)}
        manifest = dict(version=1, filenamesBySongID={sid: 'tone.wav' for sid in songs}, songsBySongID=songs,
                        artworkFilenamesByAlbumID={f'a{i:02}': 'red.png' if i == 0 else 'blue.png' for i in range(40)})
        (library / 'manifest.json').write_text(json.dumps(manifest))
        config = root / 'config' / 'akari'
        config.mkdir(parents=True)
        (config / 'config.toml').write_text(f'[storage]\nmusic_dir="{root / "music"}"\nexternal_dirs=["{library}"]\n[player]\nspatial_audio=false\n')
        state_dir = root / 'cache' / 'akari' / NAMESPACE
        state_dir.mkdir(parents=True)
        (state_dir / 'playback.json').write_text(json.dumps(dict(queue=[], index=-1, volume=0, position=0)))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 120, 1200, 720))
        before = termios.tcgetattr(slave)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'), TERM='xterm-kitty', TERM_PROGRAM=program)
        env.pop('TMUX', None)
        env.pop('NO_COLOR', None)
        env['COLORTERM'] = 'truecolor'
        process = subprocess.Popen([BINARY], stdin=slave, stdout=slave, stderr=slave, env=env)

        def read_for(seconds):
            output = bytearray()
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                if select.select([master], [], [], min(.05, max(0, until-time.monotonic())))[0]:
                    try:
                        output.extend(os.read(master, 65536))
                    except OSError:
                        break
            assert process.poll() is None, output.decode(errors='replace')
            return bytes(output)

        def send(keys, delay=.4):
            os.write(master, keys)
            return read_for(delay)

        def no_upload(output):
            assert b'a=t,t=d' not in output, 'cached cover was uploaded again'

        try:
            initial = read_for(1.5)
            assert b'COVER' in initial and b'PULSE' in initial, 'missing artwork/spectrum panels'
            assert initial.count(b'a=t,t=d') == 1, 'initial cover did not upload exactly once'
            scroll = send(b'\x1b[<65;5;8M' * 8)
            assert b'\x1b_G' not in scroll, 'trackpad scroll changed Kitty image layer'
            send(b'\r')  # Still Album 00: scrolling moved only the viewport.
            playing = send(b'\r', 1.8)
            no_upload(playing)
            assert any(block.encode() in playing for block in '▁▂▃▄▅▆▇█'), 'real FFT produced no bars'
            send(b'\x1b', .3)
            blue_cover = send(b'j', .8)
            assert blue_cover.count(b'a=t,t=d') == 1, 'browsing Album 01 did not change the cover'
            animation = read_for(.8)
            no_upload(animation)
            colors = re.findall(rb'(?:38|48);2;(\d+);(\d+);(\d+)', blue_cover + animation)
            assert (b'255', b'0', b'0') in colors, f'playing album red accent disappeared: {colors}'
            assert (b'0', b'0', b'255') not in colors, 'browsed album recolored playback'
            hidden = send(b'?')
            assert b'a=d,d=i' in hidden, 'help did not hide the cover'
            no_upload(send(b'\x1b'))
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 128, 1280, 800))
            process.send_signal(signal.SIGWINCH)
            resized = read_for(.4)
            if program == 'rex':
                assert resized.count(b'a=t,t=d') == 1, 'Rex surface resize did not restore the current cover'
            else:
                no_upload(resized)
            assert b'a=p,' in resized, 'resize did not reposition the cached cover'
            no_upload(send(b'k'))  # Return to Album 00: terminal GPU cache hit.
            send(b'\x1b[O', .1)  # Terminal detaches/rebuilds its graphics layer.
            recovered = send(b'\x1b[I')
            assert recovered.count(b'a=t,t=d') == 1, 'focus recovery must restore only the current cover'
            assert b'\x1b_G' not in send(b'\x1b[<65;5;8M'), 'scroll repainted the recovered cover'
            os.write(master, b'q')
            process.wait(timeout=8)
            assert process.returncode == 0
            assert before == termios.tcgetattr(slave), 'terminal mode was not restored'
            state = json.loads((state_dir / 'playback.json').read_text())
            assert state['queue'][state['index']]['albumID'] == 'a00', 'scroll moved the selection'
            assert (library / 'red.png').read_bytes() == png([255, 0, 0]), 'external artwork was modified'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)


cover_and_spectrum_flow()
cover_and_spectrum_flow('rex')
print('PASS Kitty cover stays fixed on scroll/FFT, GPU reuse, help, resize, independent playing palette')
