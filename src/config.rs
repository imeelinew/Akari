use crate::core::Config;
use anyhow::{Context, Result, bail};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if path.starts_with("~/") || path.starts_with("~\\") {
        home.join(&path[2..])
    } else {
        PathBuf::from(path)
    }
}
pub fn load() -> Result<Config> {
    let home = PathBuf::from(env::var_os("HOME").context("determine home directory")?);
    let root = env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| expand_home(&s, &home))
        .unwrap_or_else(|| home.join(".config"));
    load_at(&root.join("akari/config.toml"), &home)
}
fn load_at(path: &Path, home: &Path) -> Result<Config> {
    let mut default = Config::default();
    default.storage.music_dir = home
        .join(".local/share/akari/music")
        .to_string_lossy()
        .into_owned();
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(path.parent().context("config parent")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(toml::to_string_pretty(&default)?.as_bytes())?;
            file.sync_all()?;
            bail!(
                "Created {}; configure [server] or [storage].external_dirs, then run akari",
                path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("create config"),
    }
    let data = fs::read_to_string(path).context("read config")?;
    let mut cfg: Config = toml::from_str(&data).map_err(|_| {
        anyhow::anyhow!(
            "parse config {}: invalid TOML configuration",
            path.display()
        )
    })?;
    if cfg.storage.music_dir.trim().is_empty() {
        cfg.storage.music_dir = default.storage.music_dir;
    }
    for value in
        std::iter::once(&mut cfg.storage.music_dir).chain(cfg.storage.external_dirs.iter_mut())
    {
        let p = expand_home(value, home);
        *value = if p.is_absolute() {
            p
        } else {
            path.parent().unwrap().join(p)
        }
        .to_string_lossy()
        .into_owned();
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_launch_and_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(
            load_at(&path, tmp.path())
                .unwrap_err()
                .to_string()
                .starts_with("Created")
        );
        assert!(load_at(&path, tmp.path()).unwrap().player.spatial_audio);
        fs::write(
            &path,
            "[storage]\nmusic_dir='music'\nexternal_dirs=['~/Music','../albums']",
        )
        .unwrap();
        let cfg = load_at(&path, tmp.path()).unwrap();
        assert_eq!(
            cfg.storage.music_dir,
            tmp.path().join("music").to_string_lossy()
        );
        assert_eq!(
            cfg.storage.external_dirs[0],
            tmp.path().join("Music").to_string_lossy()
        );
    }
    #[test]
    fn config_errors_do_not_expose_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "password = secret-value invalid").unwrap();
        assert!(
            !load_at(&path, tmp.path())
                .unwrap_err()
                .to_string()
                .contains("secret-value")
        );
    }
}
