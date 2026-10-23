//! AVFoundation player bridge.
//!
//! AVFoundation expects a live process main run loop, while the application can
//! keep its own work on a background thread. Call [`run`] from the process main
//! thread and create/use [`Player`] from inside its work closure.

use anyhow::{Result, bail};
#[cfg(target_os = "macos")]
use std::ffi::CString;

/// A point-in-time view of native playback state.
///
/// This is copied from the Objective-C bridge and is safe to poll every 200 ms.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub generation: u64,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub status: i32,
    pub paused: bool,
    pub buffering: bool,
    pub ended: bool,
    pub spatial: bool,
    pub error_code: i32,
}

#[cfg(target_os = "macos")]
mod ffi {
    use super::{Snapshot, finite_clamped, finite_non_negative};
    use std::ffi::{c_char, c_double, c_int, c_void};

    #[repr(C)]
    pub struct NativeSnapshot {
        pub generation: u64,
        pub position: c_double,
        pub duration: c_double,
        pub volume: c_double,
        pub status: c_int,
        pub paused: c_int,
        pub buffering: c_int,
        pub ended: c_int,
        pub spatial: c_int,
        pub error_code: c_int,
    }

    impl Default for NativeSnapshot {
        fn default() -> Self {
            Self {
                generation: 0,
                position: 0.0,
                duration: 0.0,
                volume: 0.0,
                status: 0,
                paused: 0,
                buffering: 0,
                ended: 0,
                spatial: 0,
                error_code: 0,
            }
        }
    }

    impl NativeSnapshot {
        pub fn into_public(self) -> Snapshot {
            Snapshot {
                generation: self.generation,
                position: finite_non_negative(self.position),
                duration: finite_non_negative(self.duration),
                volume: finite_clamped(self.volume, 0.0, 100.0),
                status: self.status,
                paused: self.paused != 0,
                buffering: self.buffering != 0,
                ended: self.ended != 0,
                spatial: self.spatial != 0,
                error_code: self.error_code,
            }
        }
    }

    unsafe extern "C" {
        pub fn ak_main_prepare();
        pub fn ak_main_run();
        pub fn ak_main_stop();
        pub fn ak_main_is_main_thread() -> c_int;
        pub fn ak_player_create(spatial: c_int) -> *mut c_void;
        pub fn ak_player_load(
            handle: *mut c_void,
            source: *const c_char,
            paused: c_int,
            position: c_double,
            generation: u64,
        ) -> c_int;
        pub fn ak_player_pause(handle: *mut c_void, paused: c_int);
        pub fn ak_player_seek(handle: *mut c_void, seconds: c_double, absolute: c_int);
        pub fn ak_player_volume(handle: *mut c_void, volume: c_double);
        pub fn ak_player_spatial(handle: *mut c_void, enabled: c_int);
        pub fn ak_player_stop(handle: *mut c_void, generation: u64);
        pub fn ak_player_snapshot(handle: *mut c_void) -> NativeSnapshot;
        pub fn ak_player_destroy(handle: *mut c_void);
    }
}

/// Runs `work` on a background thread while retaining the main CFRunLoop.
///
/// A panic is caught long enough to stop the main loop, then resumed on the
/// calling thread so normal Rust panic handling is preserved.
#[cfg(target_os = "macos")]
pub fn run(work: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::thread;

    struct StopMainLoop;

    impl Drop for StopMainLoop {
        fn drop(&mut self) {
            // SAFETY: this only sets a process-local atomic flag and wakes the
            // loop owned by the calling thread.
            unsafe { ffi::ak_main_stop() };
        }
    }

    // SAFETY: this only queries the calling NSThread.
    if unsafe { ffi::ak_main_is_main_thread() } == 0 {
        bail!("player::run must be called from the macOS main thread");
    }

    // SAFETY: run owns the process main-loop lifecycle for the duration of
    // this call. prepare happens before the worker can begin dispatching work.
    unsafe { ffi::ak_main_prepare() };

    let worker = match thread::Builder::new()
        .name("akari-player-work".to_owned())
        .spawn(move || {
            let _stop_main_loop = StopMainLoop;
            catch_unwind(AssertUnwindSafe(work))
        }) {
        Ok(worker) => worker,
        Err(error) => {
            unsafe { ffi::ak_main_stop() };
            return Err(error.into());
        }
    };

    // SAFETY: run is called on the process main thread by the application.
    unsafe { ffi::ak_main_run() };

    match worker.join() {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) | Err(payload) => resume_unwind(payload),
    }
}

/// Non-macOS builds compile so callers fail with a useful error.
#[cfg(not(target_os = "macos"))]
pub fn run(work: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    let _ = work;
    bail!("Akari player requires macOS")
}

/// An AVFoundation-backed audio player.
#[cfg(target_os = "macos")]
pub struct Player {
    native: std::ptr::NonNull<std::ffi::c_void>,
    generation: u64,
    spatial: bool,
}

/// Non-macOS placeholder. Construction always returns an error.
#[cfg(not(target_os = "macos"))]
pub struct Player {
    _private: (),
}

#[cfg(target_os = "macos")]
unsafe impl Send for Player {}

#[cfg(target_os = "macos")]
impl Player {
    /// Creates an AVFoundation player.
    pub fn new(spatial: bool) -> Result<Self> {
        // SAFETY: the bridge returns either a retained native player or null.
        let pointer = unsafe { ffi::ak_player_create(i32::from(spatial)) };
        let native = std::ptr::NonNull::new(pointer)
            .ok_or_else(|| anyhow::anyhow!("cannot initialize native audio"))?;
        Ok(Self {
            native,
            generation: 0,
            spatial,
        })
    }

    /// Replaces the current source and starts loading it.
    pub fn load(&mut self, source: &str, paused: bool, position: f64) -> Result<()> {
        if !valid_source(source) {
            bail!("invalid audio source");
        }

        let source = CString::new(source).map_err(|_| anyhow::anyhow!("invalid audio source"))?;
        let position = finite_non_negative(position);
        let generation = self.generation.wrapping_add(1);

        // SAFETY: native is a live retained pointer and source is NUL
        // terminated for the duration of the call.
        let loaded = unsafe {
            ffi::ak_player_load(
                self.native.as_ptr(),
                source.as_ptr(),
                i32::from(paused),
                position,
                generation,
            )
        };
        if loaded == 0 {
            bail!("cannot open audio source");
        }

        self.generation = generation;
        Ok(())
    }

    /// Pauses or resumes playback.
    pub fn pause(&mut self, paused: bool) -> Result<()> {
        // SAFETY: native is live for the lifetime of self.
        unsafe { ffi::ak_player_pause(self.native.as_ptr(), i32::from(paused)) };
        Ok(())
    }

    /// Seeks to an absolute position or by a relative offset, in seconds.
    pub fn seek(&mut self, seconds: f64, absolute: bool) -> Result<()> {
        if !seconds.is_finite() {
            bail!("invalid seek position");
        }
        // SAFETY: native is live for the lifetime of self.
        unsafe {
            ffi::ak_player_seek(self.native.as_ptr(), seconds, i32::from(absolute));
        }
        Ok(())
    }

    /// Sets volume on AVFoundation's 0 through 100 scale.
    pub fn volume(&mut self, volume: f64) -> Result<()> {
        if !volume.is_finite() {
            bail!("invalid volume");
        }
        let volume = volume.clamp(0.0, 100.0);
        // SAFETY: native is live for the lifetime of self.
        unsafe { ffi::ak_player_volume(self.native.as_ptr(), volume) };
        Ok(())
    }

    /// Enables or disables Apple's audio spatialization for the current item.
    pub fn set_spatial(&mut self, enabled: bool) -> Result<()> {
        // SAFETY: native is live for the lifetime of self.
        unsafe { ffi::ak_player_spatial(self.native.as_ptr(), i32::from(enabled)) };
        self.spatial = enabled;
        Ok(())
    }

    /// Stops playback and advances the generation.
    pub fn stop(&mut self) -> Result<()> {
        let generation = self.generation.wrapping_add(1);
        // SAFETY: native is live for the lifetime of self.
        unsafe { ffi::ak_player_stop(self.native.as_ptr(), generation) };
        self.generation = generation;
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn spatial(&self) -> bool {
        self.spatial
    }

    pub fn snapshot(&self) -> Snapshot {
        // SAFETY: native is live for the lifetime of self. The C function
        // returns a value struct whose layout is asserted on both sides.
        let snapshot = unsafe { ffi::ak_player_snapshot(self.native.as_ptr()) };
        snapshot.into_public()
    }
}

#[cfg(target_os = "macos")]
impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: Drop owns the retained pointer and calls it exactly once.
        unsafe { ffi::ak_player_destroy(self.native.as_ptr()) };
    }
}

#[cfg(not(target_os = "macos"))]
impl Player {
    pub fn new(_spatial: bool) -> Result<Self> {
        bail!("Akari player requires macOS")
    }

    pub fn load(&mut self, _source: &str, _paused: bool, _position: f64) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn pause(&mut self, _paused: bool) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn seek(&mut self, _seconds: f64, _absolute: bool) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn volume(&mut self, _volume: f64) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn set_spatial(&mut self, _enabled: bool) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn stop(&mut self) -> Result<()> {
        bail!("Akari player requires macOS")
    }

    pub fn generation(&self) -> u64 {
        0
    }

    pub fn spatial(&self) -> bool {
        false
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }
}

#[cfg(target_os = "macos")]
fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(target_os = "macos")]
fn finite_clamped(value: f64, minimum: f64, maximum: f64) -> f64 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        minimum
    }
}

#[cfg(target_os = "macos")]
fn valid_source(source: &str) -> bool {
    if source.is_empty() || source.as_bytes().contains(&0) {
        return false;
    }
    if std::path::Path::new(source).is_absolute() {
        return true;
    }
    valid_http_source(source)
}

#[cfg(target_os = "macos")]
fn valid_http_source(source: &str) -> bool {
    let remainder = source
        .strip_prefix("http://")
        .or_else(|| source.strip_prefix("https://"));
    let Some(remainder) = remainder else {
        return false;
    };
    if remainder
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }

    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() {
        return false;
    }

    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host_port.starts_with('[') {
        let Some(close) = host_port.find(']') else {
            return false;
        };
        if close <= 1 {
            return false;
        }
        let suffix = &host_port[close + 1..];
        suffix.is_empty()
            || suffix
                .strip_prefix(':')
                .is_some_and(|port| !port.is_empty())
    } else {
        !host_port.is_empty() && !host_port.starts_with(':')
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_matches_objective_c_layout() {
        use super::ffi::NativeSnapshot;
        use std::mem::{align_of, size_of};

        assert_eq!(size_of::<NativeSnapshot>(), 56);
        assert_eq!(align_of::<NativeSnapshot>(), 8);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, generation), 0);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, position), 8);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, duration), 16);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, volume), 24);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, status), 32);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, paused), 36);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, buffering), 40);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, ended), 44);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, spatial), 48);
        assert_eq!(std::mem::offset_of!(NativeSnapshot, error_code), 52);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn validates_supported_sources() {
        assert!(super::valid_source("/tmp/Akari Test.flac"));
        assert!(super::valid_source("http://localhost:8080/audio"));
        assert!(super::valid_source("https://example.com/a?b=c#d"));
        assert!(super::valid_source("https://[::1]:8443/a"));

        assert!(!super::valid_source(""));
        assert!(!super::valid_source("relative/file.flac"));
        assert!(!super::valid_source("file:///tmp/file.flac"));
        assert!(!super::valid_source("ftp://example.com/file.flac"));
        assert!(!super::valid_source("https:///file.flac"));
        assert!(!super::valid_source("https://exa mple.com/file.flac"));
        assert!(!super::valid_source("https://example.com/\0file.flac"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normalizes_load_position() {
        assert_eq!(super::finite_non_negative(f64::NAN), 0.0);
        assert_eq!(super::finite_non_negative(f64::INFINITY), 0.0);
        assert_eq!(super::finite_non_negative(-5.0), 0.0);
        assert_eq!(super::finite_non_negative(5.0), 5.0);
    }
}
