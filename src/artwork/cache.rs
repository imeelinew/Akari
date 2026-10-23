use super::{Image, Palette};
use crate::artwork::palette::palette_for;
use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, ImageReader, RgbaImage};
use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::NamedTempFile;
use url::Url;

const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_INPUT_BYTES: u64 = 12 << 20;
const MAX_INPUT_DIMENSION: u32 = 20_000;
const MAX_OUTPUT_DIMENSION: u32 = 700;
const MAX_OUTPUT_BYTES: usize = 4 << 20;
const MEMORY_CACHE_ITEMS: usize = 64;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const READ_BUFFER_BYTES: usize = 64 << 10;

/// Loads, normalizes, and caches cover artwork.
pub struct Cache {
    cache_dir: PathBuf,
    client: Client,
    memory: Mutex<MemoryCache>,
}

struct MemoryCache {
    items: HashMap<String, Arc<Image>>,
    order: VecDeque<String>,
}

impl Cache {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            client: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("static HTTP client configuration"),
            memory: Mutex::new(MemoryCache {
                items: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    #[allow(dead_code)]
    pub fn load(&self, key: &str, source: &str) -> Result<Arc<Image>> {
        self.load_cancellable(key, source, &AtomicBool::new(false))
    }

    /// Loads artwork while allowing a superseded request to stop between I/O chunks.
    ///
    /// Cancellation is best effort. The blocking HTTP request itself remains bounded by
    /// the client's 15 second timeout.
    pub fn load_cancellable(
        &self,
        key: &str,
        source: &str,
        cancel: &AtomicBool,
    ) -> Result<Arc<Image>> {
        check_cancel(cancel)?;
        let cache_key = hex_sha256(key.as_bytes());
        if let Some(image) = self.memory_get(&cache_key) {
            return Ok(image);
        }

        if let Some(image) = self.disk_get(&cache_key, cancel)? {
            self.memory_put(cache_key, image.clone());
            return Ok(image);
        }

        let mut raw = self.read_source(source, cancel)?;
        let image = normalize(&raw, cancel);
        raw.fill(0);
        let image = image?;
        check_cancel(cancel)?;

        let image = Arc::new(image);
        self.memory_put(cache_key.clone(), image.clone());
        let _ = self.disk_put(&cache_key, &image.png);
        Ok(image)
    }

    fn memory_get(&self, key: &str) -> Option<Arc<Image>> {
        let mut cache = lock(&self.memory);
        let image = cache.items.get(key).cloned()?;
        cache.order.retain(|candidate| candidate != key);
        cache.order.push_front(key.to_owned());
        Some(image)
    }

    fn memory_put(&self, key: String, image: Arc<Image>) {
        let mut cache = lock(&self.memory);
        cache.items.insert(key.clone(), image);
        cache.order.retain(|candidate| candidate != &key);
        cache.order.push_front(key);
        while cache.order.len() > MEMORY_CACHE_ITEMS {
            if let Some(oldest) = cache.order.pop_back() {
                cache.items.remove(&oldest);
            }
        }
    }

    fn read_source(&self, source: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
        let parsed = Url::parse(source).ok();
        if let Some(url) = parsed.as_ref()
            && !url.scheme().is_empty()
        {
            return match url.scheme().to_ascii_lowercase().as_str() {
                "http" | "https" if url.host_str().is_some() => self.read_http(source, cancel),
                "http" | "https" => bail!("artwork: HTTP source has no host"),
                scheme => bail!("artwork: unsupported source scheme {scheme:?}"),
            };
        }

        let path = Path::new(source);
        if !path.is_absolute() {
            bail!("artwork: local source must be an absolute path");
        }
        read_file(path, cancel)
    }

    fn read_http(&self, source: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
        check_cancel(cancel)?;
        let response = self
            .client
            .get(source)
            .header("Accept", "image/jpeg,image/png,image/gif")
            .send()
            .map_err(|_| anyhow!("artwork: request failed"))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            drain_bounded_response(response, 64 << 10);
            bail!("artwork: HTTP status {status}");
        }
        read_response(response, cancel)
    }

    fn disk_get(&self, key: &str, cancel: &AtomicBool) -> Result<Option<Arc<Image>>> {
        if self.cache_dir.as_os_str().is_empty() {
            return Ok(None);
        }
        let path = self.cache_dir.join(format!("{key}.png"));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Ok(None),
        };
        if !metadata.file_type().is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_OUTPUT_BYTES as u64
            || !private_file(&metadata)
        {
            remove_invalid_disk_file(&path);
            return Ok(None);
        }

        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => {
                remove_invalid_disk_file(&path);
                return Ok(None);
            }
        };
        let data = match read_limited(&mut file, MAX_OUTPUT_BYTES as u64, cancel) {
            Ok(data) => data,
            Err(error) => {
                check_cancel(cancel)?;
                let _ = error;
                remove_invalid_disk_file(&path);
                return Ok(None);
            }
        };
        check_cancel(cancel)?;

        let dimensions = match image_dimensions(&data) {
            Ok((width, height, ImageFormat::Png)) => (width, height),
            _ => {
                remove_invalid_disk_file(&path);
                return Ok(None);
            }
        };
        if dimensions.0 == 0
            || dimensions.1 == 0
            || dimensions.0 > MAX_OUTPUT_DIMENSION
            || dimensions.1 > MAX_OUTPUT_DIMENSION
        {
            remove_invalid_disk_file(&path);
            return Ok(None);
        }

        check_cancel(cancel)?;
        let decoded = match image::load_from_memory_with_format(&data, ImageFormat::Png) {
            Ok(decoded) if decoded.width() == dimensions.0 && decoded.height() == dimensions.1 => {
                decoded
            }
            _ => {
                remove_invalid_disk_file(&path);
                return Ok(None);
            }
        };
        check_cancel(cancel)?;
        let palette = palette_for(&decoded);
        Ok(Some(Arc::new(Image {
            png: data,
            width: dimensions.0,
            height: dimensions.1,
            palette,
        })))
    }

    fn disk_put(&self, key: &str, data: &[u8]) -> Result<()> {
        if self.cache_dir.as_os_str().is_empty() {
            return Ok(());
        }
        ensure_cache_dir(&self.cache_dir)?;
        let path = self.cache_dir.join(format!("{key}.png"));
        let mut temporary = NamedTempFile::new_in(&self.cache_dir)
            .context("create temporary artwork cache file")?;
        set_private_file(temporary.as_file())?;
        temporary
            .write_all(data)
            .context("write artwork cache file")?;
        temporary
            .as_file_mut()
            .flush()
            .context("flush artwork cache file")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync artwork cache file")?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .context("install artwork cache file")?;
        if let Ok(directory) = File::open(&self.cache_dir) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    #[cfg(test)]
    fn memory_len(&self) -> usize {
        lock(&self.memory).items.len()
    }
}

fn normalize(raw: &[u8], cancel: &AtomicBool) -> Result<Image> {
    check_cancel(cancel)?;
    let (width, height, format) =
        image_dimensions(raw).context("artwork: decode image header failed")?;
    if !matches!(
        format,
        ImageFormat::Gif | ImageFormat::Jpeg | ImageFormat::Png
    ) {
        bail!("artwork: unsupported image format");
    }
    validate_dimensions(width, height)?;
    check_cancel(cancel)?;

    let decoded =
        image::load_from_memory_with_format(raw, format).context("artwork: decode image failed")?;
    check_cancel(cancel)?;
    if decoded.width() != width || decoded.height() != height {
        bail!("artwork: decoded image dimensions changed");
    }

    let (output_width, output_height) = normalized_size(width, height);
    let output = if (output_width, output_height) == (width, height) {
        decoded.to_rgba8()
    } else {
        scale_down(&decoded, output_width, output_height, cancel)?
    };
    check_cancel(cancel)?;

    let normalized = DynamicImage::ImageRgba8(output);
    let palette: Palette = palette_for(&normalized);
    check_cancel(cancel)?;
    let mut png = Vec::new();
    normalized
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .context("artwork: encode PNG failed")?;
    if png.len() > MAX_OUTPUT_BYTES {
        bail!("artwork: encoded image exceeds {MAX_OUTPUT_BYTES} bytes");
    }
    check_cancel(cancel)?;
    Ok(Image {
        png,
        width: output_width,
        height: output_height,
        palette,
    })
}

fn image_dimensions(raw: &[u8]) -> Result<(u32, u32, ImageFormat)> {
    let reader = ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .context("artwork: inspect image format failed")?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("artwork: unknown image format"))?;
    let (width, height) = reader
        .into_dimensions()
        .context("artwork: inspect image dimensions failed")?;
    Ok((width, height, format))
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("artwork: image has invalid dimensions");
    }
    if width > MAX_INPUT_DIMENSION || height > MAX_INPUT_DIMENSION {
        bail!("artwork: image dimensions exceed limit");
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        bail!("artwork: image contains more than {MAX_IMAGE_PIXELS} pixels");
    }
    Ok(())
}

fn normalized_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_OUTPUT_DIMENSION {
        return (width, height);
    }
    let scaled_width = (u64::from(width) * u64::from(MAX_OUTPUT_DIMENSION)
        + u64::from(longest) / 2)
        / u64::from(longest);
    let scaled_height = (u64::from(height) * u64::from(MAX_OUTPUT_DIMENSION)
        + u64::from(longest) / 2)
        / u64::from(longest);
    ((scaled_width.max(1)) as u32, (scaled_height.max(1)) as u32)
}

fn scale_down(
    source: &DynamicImage,
    width: u32,
    height: u32,
    cancel: &AtomicBool,
) -> Result<RgbaImage> {
    let source = source.to_rgba8();
    let source_width = source.width();
    let source_height = source.height();
    let cells = u64::from(width) * u64::from(height);
    let mut sums = vec![[0_u64; 5]; cells as usize];

    for y in 0..source_height {
        if y & 31 == 0 {
            check_cancel(cancel)?;
        }
        let destination_y = u64::from(y) * u64::from(height) / u64::from(source_height);
        for x in 0..source_width {
            let destination_x = u64::from(x) * u64::from(width) / u64::from(source_width);
            let offset = (destination_y * u64::from(width) + destination_x) as usize;
            let pixel = source.get_pixel(x, y).0;
            let sum = &mut sums[offset];
            for channel in 0..4 {
                sum[channel] += u64::from(pixel[channel]);
            }
            sum[4] += 1;
        }
    }
    check_cancel(cancel)?;

    let mut output = RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let sum = sums[(u64::from(y) * u64::from(width) + u64::from(x)) as usize];
            if sum[4] == 0 {
                continue;
            }
            output.put_pixel(
                x,
                y,
                image::Rgba([
                    (sum[0] / sum[4]) as u8,
                    (sum[1] / sum[4]) as u8,
                    (sum[2] / sum[4]) as u8,
                    (sum[3] / sum[4]) as u8,
                ]),
            );
        }
    }
    Ok(output)
}

fn read_file(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut file = File::open(path).context("artwork: open local source failed")?;
    let metadata = file
        .metadata()
        .context("artwork: inspect local source failed")?;
    if !metadata.is_file() {
        bail!("artwork: local source is not a regular file");
    }
    if metadata.len() > MAX_INPUT_BYTES {
        bail!("artwork: source exceeds {MAX_INPUT_BYTES} bytes");
    }
    read_limited(&mut file, MAX_INPUT_BYTES, cancel)
}

fn read_response(mut response: Response, cancel: &AtomicBool) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INPUT_BYTES)
    {
        bail!("artwork: source exceeds {MAX_INPUT_BYTES} bytes");
    }
    read_limited(&mut response, MAX_INPUT_BYTES, cancel)
}

fn drain_bounded_response(mut response: Response, limit: u64) {
    let _ = io::copy(&mut response.by_ref().take(limit), &mut io::sink());
}

fn read_limited(reader: &mut impl Read, limit: u64, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let mut output = Vec::with_capacity(READ_BUFFER_BYTES);
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        check_cancel(cancel)?;
        let read = reader
            .read(&mut buffer)
            .context("artwork: read source failed")?;
        if read == 0 {
            break;
        }
        if output.len() as u64 + read as u64 > limit {
            bail!("artwork: source exceeds {limit} bytes");
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(output)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        bail!("artwork: load cancelled");
    }
    Ok(())
}

fn hex_sha256(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(data);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn ensure_cache_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            bail!("artwork: cache path is not a directory");
        }
    } else {
        fs::create_dir_all(path).context("create artwork cache directory failed")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("secure artwork cache directory failed")?;
    }
    Ok(())
}

fn set_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("secure artwork cache file failed")?;
    }
    Ok(())
}

fn private_file(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn remove_invalid_disk_file(path: &Path) {
    let _ = fs::remove_file(path);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::{io::Cursor, sync::atomic::AtomicBool, thread};
    use tempfile::tempdir;
    use tiny_http::{Response as HttpResponse, Server};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 40, 30, 255]));
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut output, ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn cache_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Cache>();
    }

    #[test]
    fn memory_hit_reuses_the_arc_without_reading_the_source() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("cover.png");
        fs::write(&source, png(32, 48)).unwrap();
        let cache = Cache::new(directory.path().join("cache"));

        let first = cache.load("album", source.to_str().unwrap()).unwrap();
        fs::remove_file(&source).unwrap();
        let second = cache.load("album", source.to_str().unwrap()).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!((second.width, second.height), (32, 48));
        assert_eq!(cache.memory_len(), 1);
    }

    #[test]
    fn disk_cache_survives_a_new_cache_instance() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("cover.png");
        let cache_dir = directory.path().join("cache");
        fs::write(&source, png(40, 60)).unwrap();

        let first = Cache::new(cache_dir.clone())
            .load("same-key", source.to_str().unwrap())
            .unwrap();
        fs::remove_file(&source).unwrap();
        let second = Cache::new(cache_dir.clone())
            .load("same-key", source.to_str().unwrap())
            .unwrap();

        assert_eq!(first.png, second.png);
        assert!(
            cache_dir
                .join(format!("{}.png", hex_sha256(b"same-key")))
                .is_file()
        );
    }

    #[test]
    fn oversized_input_dimensions_are_rejected_before_decode() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("wide.png");
        fs::write(&source, png(MAX_INPUT_DIMENSION + 1, 1)).unwrap();
        let cache = Cache::new(PathBuf::new());

        let error = cache
            .load("wide", source.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("dimensions"));
    }

    #[test]
    fn long_edge_is_scaled_to_seven_hundred_pixels() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("tall.png");
        fs::write(&source, png(20, 1_400)).unwrap();
        let cache = Cache::new(PathBuf::new());

        let image = cache.load("tall", source.to_str().unwrap()).unwrap();
        assert_eq!((image.width, image.height), (10, 700));
    }

    #[test]
    fn cancellation_stops_before_source_work() {
        let cache = Cache::new(PathBuf::new());
        let cancel = AtomicBool::new(true);
        let error = cache
            .load_cancellable("cancelled", "/does/not/exist", &cancel)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelled"));
    }

    #[test]
    fn http_errors_do_not_expose_the_authenticated_url() {
        let server = Server::http(("127.0.0.1", 0)).unwrap();
        let address = server.server_addr();
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            request.respond(HttpResponse::empty(401)).unwrap();
        });
        let cache = Cache::new(PathBuf::new());
        let source = format!("http://{address}/cover?token=private-token");
        let error = cache.load("remote", &source).unwrap_err().to_string();
        worker.join().unwrap();

        assert!(!error.contains("private-token"));
        assert!(!error.contains(&address.to_string()));
    }
}
