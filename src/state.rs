use crate::core::Song;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io::Write, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackState {
    #[serde(deserialize_with = "nullable_queue")]
    pub queue: Vec<Song>,
    pub index: i32,
    pub position: f64,
    pub volume: f64,
    pub shuffle: bool,
    pub repeat: i32,
    #[serde(deserialize_with = "nullable_recent")]
    pub recent: HashMap<String, f64>,
}
impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            queue: vec![],
            index: -1,
            position: 0.,
            volume: 70.,
            shuffle: false,
            repeat: 0,
            recent: HashMap::new(),
        }
    }
}
impl PlaybackState {
    pub fn read(dir: &Path) -> Self {
        let mut state: Self = fs::read(dir.join("playback.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        state.volume = if state.volume.is_finite() {
            state.volume.clamp(0., 100.)
        } else {
            70.
        };
        if !state.position.is_finite() || state.position < 0. {
            state.position = 0.;
        }
        state.repeat = state.repeat.clamp(0, 2);
        if state.index < 0 || state.index as usize >= state.queue.len() {
            state.index = -1;
        }
        state
    }
    pub fn write(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut file, self)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(dir.join("playback.json"))?;
        Ok(())
    }
    pub fn current(&self) -> Option<&Song> {
        usize::try_from(self.index)
            .ok()
            .and_then(|i| self.queue.get(i))
    }
}

fn nullable_queue<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Song>, D::Error> {
    Ok(Option::<Vec<Song>>::deserialize(d)?.unwrap_or_default())
}
fn nullable_recent<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<HashMap<String, f64>, D::Error> {
    Ok(Option::<HashMap<String, f64>>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn go_state_compatibility_and_validation() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("playback.json"), r#"{"queue":[{"id":"s","albumID":"a"}],"index":0,"volume":150,"position":42,"repeat":2,"recent":{"a":123}}"#).unwrap();
        let s = PlaybackState::read(d.path());
        assert_eq!(s.volume, 100.);
        assert_eq!(s.current().unwrap().album_id, "a");
        s.write(d.path()).unwrap();
        assert_eq!(PlaybackState::read(d.path()).position, 42.);
        fs::write(
            d.path().join("playback.json"),
            r#"{"index":10,"position":-5}"#,
        )
        .unwrap();
        let s = PlaybackState::read(d.path());
        assert_eq!(s.index, -1);
        assert_eq!(s.position, 0.);
    }
}
