//! Artwork work is coalesced onto one worker; terminal painting never decodes images.
use crate::{
    artwork::{Cache, Image, NEUTRAL_PALETTE, Palette},
    core::Album,
    library::Library,
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Slot {
    pub album: Option<Album>,
    pub image: Option<Arc<Image>>,
    pub loading: bool,
    generation: u64,
    attempts: u8,
    retry: Option<Instant>,
    cancel: Arc<AtomicBool>,
}
struct Job {
    target: usize,
    album: Album,
    generation: u64,
    sources: Vec<String>,
    cancel: Arc<AtomicBool>,
}
struct Pending {
    jobs: [Option<Job>; 2],
    stop: bool,
}
struct Finished {
    target: usize,
    key: String,
    generation: u64,
    image: Option<Arc<Image>>,
}
pub struct Visuals {
    pub cover: Slot,
    pub playing: Slot,
    pub revision: u64,
    pending: Arc<(Mutex<Pending>, Condvar)>,
    rx: mpsc::Receiver<Finished>,
    lib: Arc<Library>,
    pub levels: [f32; 32],
}
impl Visuals {
    pub fn new(lib: Arc<Library>) -> Self {
        let pending = Arc::new((
            Mutex::new(Pending {
                jobs: [None, None],
                stop: false,
            }),
            Condvar::new(),
        ));
        let (tx, rx) = mpsc::channel();
        let work = pending.clone();
        let library = lib.clone();
        thread::Builder::new()
            .name("akari-artwork".into())
            .spawn(move || {
                let cache = Cache::new(library.cache_dir().join("artwork"));
                loop {
                    let job = {
                        let (lock, wake) = &*work;
                        let mut state = lock.lock().unwrap();
                        while !state.stop && state.jobs.iter().all(Option::is_none) {
                            state = wake.wait(state).unwrap();
                        }
                        if state.stop {
                            break;
                        }
                        state.jobs[1]
                            .take()
                            .or_else(|| state.jobs[0].take())
                            .unwrap()
                    };
                    let mut image = None;
                    for source in &job.sources {
                        if job.cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(found) =
                            cache.load_cancellable(&job.album.id, source, &job.cancel)
                        {
                            image = Some(found);
                            break;
                        }
                    }
                    if job.cancel.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Some(image) = &image {
                        let _ = library.save_artwork(&job.album.id, &image.png);
                    }
                    if tx
                        .send(Finished {
                            target: job.target,
                            key: job.album.id,
                            generation: job.generation,
                            image,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("start artwork worker");
        Self {
            cover: Slot::default(),
            playing: Slot::default(),
            revision: 0,
            pending,
            rx,
            lib,
            levels: [0.; 32],
        }
    }
    pub fn sync(&mut self, cover: Option<Album>, playing: Option<Album>) {
        for (target, album) in [cover, playing].into_iter().enumerate() {
            let slot = if target == 0 {
                &mut self.cover
            } else {
                &mut self.playing
            };
            let changed = slot.album.as_ref().map(|a| &a.id) != album.as_ref().map(|a| &a.id);
            if changed {
                slot.cancel.store(true, Ordering::Relaxed);
                slot.generation = slot.generation.wrapping_add(1);
                slot.image = None;
                slot.attempts = 0;
                slot.retry = None;
                slot.loading = false;
                self.revision = self.revision.wrapping_add(1);
                if target == 1 {
                    self.levels.fill(0.);
                }
            }
            slot.album = album;
            if changed || slot.retry.is_some_and(|time| Instant::now() >= time) {
                slot.retry = None;
                let (lock, wake) = &*self.pending;
                let mut state = lock.lock().unwrap();
                state.jobs[target] = None;
                if let Some(album) = &slot.album {
                    let mut sources = Vec::new();
                    if let Some(source) = self.lib.artwork_source(&album.id) {
                        sources.push(source);
                    }
                    if !album.artwork_url.is_empty() && !sources.contains(&album.artwork_url) {
                        sources.push(album.artwork_url.clone());
                    }
                    slot.cancel = Arc::new(AtomicBool::new(false));
                    slot.loading = !sources.is_empty();
                    slot.attempts += 1;
                    state.jobs[target] = Some(Job {
                        target,
                        album: album.clone(),
                        generation: slot.generation,
                        sources,
                        cancel: slot.cancel.clone(),
                    });
                    wake.notify_one();
                }
            }
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.rx.try_recv() {
            let slot = if done.target == 0 {
                &mut self.cover
            } else {
                &mut self.playing
            };
            if done.generation != slot.generation
                || slot.album.as_ref().is_none_or(|a| a.id != done.key)
            {
                continue;
            }
            slot.loading = false;
            if done.image.is_none() && slot.attempts < 3 {
                slot.retry = Some(Instant::now() + Duration::from_secs(slot.attempts as u64));
            }
            slot.image = done.image;
            self.revision = self.revision.wrapping_add(1);
            changed = true;
        }
        changed
    }
    pub fn palette(&self) -> Palette {
        self.playing
            .image
            .as_ref()
            .map_or(NEUTRAL_PALETTE, |i| i.palette)
    }
    pub fn animate(&mut self, samples: [f32; 32]) {
        for (level, input) in self.levels.iter_mut().zip(samples) {
            let value = if input.is_finite() {
                input.clamp(0., 1.)
            } else {
                0.
            };
            *level = if value > *level {
                *level + (value - *level) * 0.7
            } else {
                value.max(*level - 0.045)
            };
        }
    }
    pub fn decaying(&self) -> bool {
        self.levels.iter().any(|v| *v > 0.)
    }
}
impl Drop for Visuals {
    fn drop(&mut self) {
        self.cover.cancel.store(true, Ordering::Relaxed);
        self.playing.cancel.store(true, Ordering::Relaxed);
        let (lock, wake) = &*self.pending;
        if let Ok(mut state) = lock.lock() {
            state.stop = true;
            state.jobs = [None, None];
            wake.notify_one();
        }
    }
}
