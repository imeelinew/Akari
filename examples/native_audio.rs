#[path = "../src/player.rs"]
mod player;

use anyhow::Result;
use player::{Player, Snapshot};

fn write_wav(path: &std::path::Path) -> Result<()> {
    const SAMPLE_RATE: u32 = 44_100;
    const SAMPLE_COUNT: u32 = SAMPLE_RATE * 12 / 5;
    const CHANNELS: u16 = 1;
    const BITS_PER_SAMPLE: u16 = 16;

    let data_size = SAMPLE_COUNT * u32::from(CHANNELS) * u32::from(BITS_PER_SAMPLE / 8);
    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for sample_index in 0..SAMPLE_COUNT {
        let phase =
            std::f64::consts::TAU * 440.0 * f64::from(sample_index) / f64::from(SAMPLE_RATE);
        let sample = (phase.sin() * 24_000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
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
        if started.elapsed() > std::time::Duration::from_secs(5) {
            anyhow::bail!("timed out waiting for {description}; last spectrum: {spectrum:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn run_smoke() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let wav_path = directory.path().join("akari-native-smoke.wav");
    write_wav(&wav_path)?;
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
    disposable.load(source, false, 0.0)?;
    wait_for_snapshot(&disposable, "disposable player ready", |snapshot| {
        snapshot.status == 2
    })?;
    wait_for_spectrum(&disposable, "disposable player spectrum")?;
    std::thread::spawn(move || drop(disposable))
        .join()
        .map_err(|_| anyhow::anyhow!("non-main-thread player destruction panicked"))?;
    println!("native smoke: active tap destroyed from a non-main thread");
    Ok(())
}

fn main() -> Result<()> {
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
