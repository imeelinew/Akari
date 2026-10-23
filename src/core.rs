use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub genre: String,
    pub year: i32,
    pub last_played: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(rename = "albumID")]
    pub album_id: String,
    pub album_title: String,
    pub duration: f64,
    pub track_number: i32,
    pub disc_number: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub player: PlayerConfig,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub music_dir: String,
    pub external_dirs: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub spatial_audio: bool,
}
impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            spatial_audio: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_go_song_format() {
        let song: Song = serde_json::from_str(
            r#"{"id":"s","albumID":"a","albumTitle":"Album","trackNumber":3,"discNumber":2}"#,
        )
        .unwrap();
        assert_eq!(song.album_id, "a");
        assert_eq!(song.track_number, 3);
        assert_eq!(serde_json::to_value(song).unwrap()["albumID"], "a");
    }
}
