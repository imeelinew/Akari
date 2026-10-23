#[path = "../src/player.rs"]
mod player;

use anyhow::Result;
use player::{Player, Snapshot};

#[derive(Clone, Copy)]
enum Encoding {
    Pcm16,
    Pcm24,
    Float32,
}

fn write_wav(path: &std::path::Path, channels: u16, encoding: Encoding) -> Result<()> {
    const SAMPLE_RATE: u32 = 44_100;
    const SAMPLE_COUNT: u32 = SAMPLE_RATE * 12 / 5;
    let (format, bits): (u16, u16) = match encoding {
        Encoding::Pcm16 => (1, 16),
        Encoding::Pcm24 => (1, 24),
        Encoding::Float32 => (3, 32),
    };
    let block_align = channels * (bits / 8);
    let data_size = SAMPLE_COUNT * u32::from(block_align);
    // WAVEFORMATEXTENSIBLE describes the six-channel speaker layout explicitly.
    let extensible = channels > 2;
    let fmt_size: u32 = if extensible { 40 } else { 16 };
    let mut wav = Vec::with_capacity((28 + fmt_size + data_size) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(20 + fmt_size + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&fmt_size.to_le_bytes());
    wav.extend_from_slice(&(if extensible { 0xfffe } else { format }).to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * u32::from(block_align)).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        wav.extend_from_slice(&22u16.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(&0x3fu32.to_le_bytes()); // FL FR FC LFE BL BR
        wav.extend_from_slice(&u32::from(format).to_le_bytes());
        wav.extend_from_slice(&[0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for sample_index in 0..SAMPLE_COUNT {
        for channel in 0..channels {
            let frequency = 440.0 + 220.0 * f64::from(channel);
            let phase = std::f64::consts::TAU * frequency * f64::from(sample_index)
                / f64::from(SAMPLE_RATE);
            let sample = phase.sin() * 0.7;
            match encoding {
                Encoding::Pcm16 => {
                    wav.extend_from_slice(&((sample * 32767.0) as i16).to_le_bytes())
                }
                Encoding::Pcm24 => {
                    wav.extend_from_slice(&((sample * 8388607.0) as i32).to_le_bytes()[..3])
                }
                Encoding::Float32 => wav.extend_from_slice(&(sample as f32).to_le_bytes()),
            }
        }
    }

    std::fs::write(path, wav)?;
    Ok(())
}

fn wait_for_snapshot(
    player: &Player,
    description: &str,
    mut predicate: impl FnMut(&Snapshot) -> bool,
) -> Result<Snapshot> {
    let started = std::time::Instant::now();
    loop {
        let snapshot = player.snapshot();
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if started.elapsed() > std::time::Duration::from_secs(5) {
            anyhow::bail!("timed out waiting for {description}; last snapshot: {snapshot:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn spectrum_peak(levels: &[f32; 32]) -> f32 {
    levels
        .iter()
        .copied()
        .filter(|level| level.is_finite())
        .fold(0.0, f32::max)
}

fn assert_silent_spectrum(player: &Player, description: &str) {
    let spectrum = player.spectrum();
    assert!(
        spectrum.iter().all(|level| level.abs() <= f32::EPSILON),
        "{description} did not clear spectrum: {spectrum:?}"
    );
}

fn wait_for_spectrum(player: &Player, description: &str) -> Result<[f32; 32]> {
    let started = std::time::Instant::now();
    loop {
        let spectrum = player.spectrum();
        if spectrum_peak(&spectrum) > 0.01 {
            return Ok(spectrum);
        }
        if started.elapsed() > std::time::Duration::from_secs(15) {
            anyhow::bail!("timed out waiting for {description}; last spectrum: {spectrum:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// This exercises the real AVPlayer/tap path at volume zero, without fake levels.
fn verify_source(path: &std::path::Path, label: &str) -> Result<()> {
    let source = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("source path is not UTF-8"))?;
    let mut player = Player::new(false)?;
    player.volume(0.0)?;
    player.load(source, false, 0.0)?;
    let ready = wait_for_snapshot(&player, label, |snapshot| snapshot.status != 1)?;
    anyhow::ensure!(ready.status == 2, "{label}: load failed: {ready:?}");
    anyhow::ensure!(ready.volume.abs() < 0.1, "{label}: playback must be muted");
    wait_for_snapshot(&player, "playback advancing", |snapshot| {
        snapshot.position >= 0.1
    })?;
    let spectrum = wait_for_spectrum(&player, label)?;
    anyhow::ensure!(
        spectrum
            .iter()
            .all(|level| level.is_finite() && (0.0..=1.0).contains(level)),
        "{label}: invalid spectrum: {spectrum:?}"
    );
    let peak_band = spectrum
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map_or(0, |(band, _)| band);
    println!(
        "{label}: peak={:.3} at band {peak_band}, duration={:.3}s, volume=0",
        spectrum_peak(&spectrum),
        ready.duration
    );

    player.pause(true)?;
    wait_for_snapshot(&player, "pause", |snapshot| snapshot.paused)?;
    assert_silent_spectrum(&player, label);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_silent_spectrum(&player, label);
    player.pause(false)?;
    wait_for_spectrum(&player, &format!("{label} resumed"))?;
    player.stop()?;
    assert_silent_spectrum(&player, label);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_silent_spectrum(&player, label);
    println!("{label}: pause/stop zero; resume restored energy");
    Ok(())
}

fn run_format_regressions(directory: &std::path::Path) -> Result<()> {
    for (label, channels, encoding) in [
        ("mono-pcm16", 1, Encoding::Pcm16),
        ("stereo-pcm16", 2, Encoding::Pcm16),
        ("stereo-pcm24", 2, Encoding::Pcm24),
        ("stereo-float32", 2, Encoding::Float32),
        ("six-channel-pcm16", 6, Encoding::Pcm16),
    ] {
        let path = directory.join(format!("{label}.wav"));
        write_wav(&path, channels, encoding)?;
        verify_source(&path, label)?;
    }

    let afconvert = std::path::Path::new("/usr/bin/afconvert");
    if afconvert.exists() {
        let input = directory.join("stereo-pcm16.wav");
        for (label, extension, file_format, data_format) in [
            ("stereo-flac", "flac", "flac", "flac"),
            ("stereo-aac", "m4a", "m4af", "aac"),
        ] {
            let output = directory.join(format!("{label}.{extension}"));
            let result = std::process::Command::new(afconvert)
                .arg(&input)
                .arg(&output)
                .args(["-f", file_format, "-d", data_format, "-c", "2"])
                .output()?;
            anyhow::ensure!(
                result.status.success(),
                "{label}: afconvert failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            verify_source(&output, label)?;
        }
    } else {
        println!("native smoke: afconvert unavailable; skipping compressed fixtures");
    }
    Ok(())
}

fn run_smoke() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let wav_path = directory.path().join("akari-native-smoke.wav");
    write_wav(&wav_path, 1, Encoding::Pcm16)?;
    let source = wav_path.to_str().expect("temporary WAV path is UTF-8");

    let mut player = Player::new(true)?;
    assert_eq!(player.generation(), 0);
    assert!(player.spatial());
    let initial = player.snapshot();
    assert_eq!(initial.status, 0);
    assert!(initial.paused);
    assert!(initial.spatial);
    assert!((initial.volume - 70.0).abs() < 0.1);
    assert_silent_spectrum(&player, "initial state");

    player.volume(0.0)?;
    player.load(source, false, 0.0)?;
    assert_eq!(player.generation(), 1);
    let ready = wait_for_snapshot(&player, "AVPlayer ready", |snapshot| snapshot.status != 1)?;
    assert_eq!(ready.status, 2, "native load failed: {ready:?}");
    assert!(ready.duration > 1.0);
    assert!(!ready.paused);
    assert!(ready.spatial);
    println!("native smoke: ready, duration={:.3}s", ready.duration);

    let playing = wait_for_snapshot(&player, "playback position", |snapshot| {
        snapshot.position >= 0.1
    })?;
    assert!(playing.position < playing.duration);
    println!(
        "native smoke: position advanced to {:.3}s",
        playing.position
    );
    let spectrum = wait_for_spectrum(&player, "live audio spectrum")?;
    let peak = spectrum_peak(&spectrum);
    let peak_band = spectrum
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map_or(0, |(band, _)| band);
    assert!(
        peak > 0.01,
        "spectrum must contain real energy: {spectrum:?}"
    );
    println!("native smoke: spectrum peak={peak:.3} at band {peak_band}");

    player.pause(true)?;
    let paused = wait_for_snapshot(&player, "pause", |snapshot| snapshot.paused)?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    let paused_after = player.snapshot();
    assert!(
        (paused_after.position - paused.position).abs() < 0.15,
        "position advanced while paused: {paused:?} -> {paused_after:?}"
    );
    println!("native smoke: paused at {:.3}s", paused_after.position);
    assert_silent_spectrum(&player, "pause");
    println!("native smoke: spectrum cleared on pause");

    player.seek(0.5, true)?;
    let sought = wait_for_snapshot(&player, "seek", |snapshot| {
        (0.45..0.9).contains(&snapshot.position)
    })?;
    assert!(sought.paused);
    assert_silent_spectrum(&player, "seek");
    println!("native smoke: seek reached {:.3}s", sought.position);

    player.volume(42.0)?;
    let volume = wait_for_snapshot(&player, "volume", |snapshot| {
        (snapshot.volume - 42.0).abs() < 0.1
    })?;
    assert_eq!(volume.generation, 1);
    println!("native smoke: volume={:.1}", volume.volume);

    player.set_spatial(false)?;
    assert!(!player.spatial());
    assert!(!player.snapshot().spatial);
    println!("native smoke: spatial disabled");

    player.volume(0.0)?;
    player.pause(false)?;
    wait_for_snapshot(&player, "resume", |snapshot| !snapshot.paused)?;
    wait_for_spectrum(&player, "spectrum after resume")?;

    player.stop()?;
    assert_eq!(player.generation(), 2);
    let stopped = player.snapshot();
    assert_eq!(stopped.generation, 2);
    assert_eq!(stopped.status, 0);
    assert!(stopped.paused);
    assert_eq!(stopped.position, 0.0);
    assert_eq!(stopped.duration, 0.0);
    assert_silent_spectrum(&player, "stop");
    println!("native smoke: stopped");

    let mut disposable = Player::new(false)?;
    disposable.volume(0.0)?;
    disposable.load(source, false, 0.0)?;
    wait_for_snapshot(&disposable, "disposable player ready", |snapshot| {
        snapshot.status == 2
    })?;
    wait_for_spectrum(&disposable, "disposable player spectrum")?;
    std::thread::spawn(move || drop(disposable))
        .join()
        .map_err(|_| anyhow::anyhow!("non-main-thread player destruction panicked"))?;
    println!("native smoke: active tap destroyed from a non-main thread");
    drop(player);
    run_format_regressions(directory.path())?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    if let Some(option) = args.next() {
        anyhow::ensure!(option == "--source", "usage: native_audio [--source PATH]");
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--source requires PATH"))?;
        anyhow::ensure!(args.next().is_none(), "usage: native_audio [--source PATH]");
        let path = std::path::PathBuf::from(path).canonicalize()?;
        return player::run(move || verify_source(&path, "external source"));
    }
    player::run(run_smoke)?;

    let panic = std::panic::catch_unwind(|| {
        player::run(|| -> Result<()> {
            std::panic::panic_any("intentional native smoke panic");
        })
    });
    assert!(panic.is_err(), "worker panic must leave player::run");
    println!("native smoke: panic stopped main loop");
    Ok(())
}
