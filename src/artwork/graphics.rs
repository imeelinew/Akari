use super::Image;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    sync::atomic::{AtomicU32, Ordering},
};

const GRAPHICS_CHUNK_BYTES: usize = 3072;
const MAX_GRAPHICS_PNG_BYTES: usize = 4 << 20;
const GPU_CACHE_ITEMS: usize = 32;
static PROCESS_INSTANCE: AtomicU32 = AtomicU32::new(1);

/// Kitty graphics state with a process-local image namespace and a 32-image GPU LRU.
pub struct Graphics {
    namespace: u32,
    next_sequence: u8,
    uploads: HashMap<String, Upload>,
    upload_order: VecDeque<String>,
    displayed: Option<Displayed>,
    last_request: Option<Request>,
    force_replay: bool,
}

struct Upload {
    id: u32,
    content: ImageContent,
    width: u32,
    height: u32,
    payload: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ImageContent {
    digest: [u8; 32],
    len: usize,
    width: u32,
    height: u32,
}

#[derive(Clone, PartialEq, Eq)]
struct Request {
    key: Option<String>,
    geometry: Option<Geometry>,
    content: Option<ImageContent>,
}

struct Displayed {
    key: String,
    id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    column: u32,
    row: u32,
    columns: u32,
    rows: u32,
}

impl Graphics {
    pub fn new() -> Self {
        let instance = PROCESS_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let entropy = rand::random::<u32>();
        let namespace =
            0x8000_0000 | ((entropy ^ instance.wrapping_mul(0x0100_0000)) & 0x7fff_ff00);
        Self {
            namespace,
            next_sequence: 0,
            uploads: HashMap::new(),
            upload_order: VecDeque::new(),
            displayed: None,
            last_request: None,
            force_replay: false,
        }
    }

    /// Reports whether the current terminal exposes Kitty graphics.
    pub fn supported() -> bool {
        if env_value("TMUX").is_some() {
            return false;
        }
        let program = env_value("TERM_PROGRAM").unwrap_or_default();
        if program == "ghostty" || program == "rex" {
            return true;
        }
        let term = env_value("TERM").unwrap_or_default();
        term.contains("ghostty")
            || term == "xterm-rex"
            || term.contains("kitty")
            || env_value("KITTY_WINDOW_ID").is_some()
    }

    /// Replays placement on the next update while retaining uploaded image data.
    ///
    /// Use this after focus or resize events that reactivate the view. Calling
    /// [`Self::clear`] instead drops the GPU upload cache and causes a re-upload.
    pub fn invalidate(&mut self) {
        self.force_replay = true;
    }

    /// Discards the currently displayed upload and deletes its GPU image data.
    ///
    /// Call this for `FocusGained`, where a terminal may have detached and rebuilt
    /// its graphics layer. The next [`Self::update`] re-uploads only the displayed
    /// image; other cached uploads remain available.
    pub fn recover(&mut self) -> String {
        let Some(displayed) = self.displayed.take() else {
            return String::new();
        };
        self.uploads.remove(&displayed.key);
        self.upload_order
            .retain(|candidate| candidate != &displayed.key);
        self.last_request = None;
        self.force_replay = false;
        delete_image_data(displayed.id)
    }

    /// Updates the image layer and returns terminal bytes only when something changed.
    ///
    /// `area` uses Ratatui's zero-based coordinates; Kitty commands convert them to
    /// one-based rows and columns.
    pub fn update(
        &mut self,
        key: Option<&str>,
        image: Option<&Image>,
        area: Rect,
        cell_pixels: (u16, u16),
        visible: bool,
    ) -> String {
        let key = key.filter(|value| !value.is_empty());
        let content = image.map(ImageContent::from).or_else(|| {
            key.and_then(|key| self.uploads.get(key))
                .map(|upload| upload.content)
        });
        let upload_dimensions = image
            .map(|image| (image.width, image.height))
            .or_else(|| key.and_then(|key| self.uploads.get(key)).map(upload_dims));
        let geometry = visible
            .then_some(())
            .and(upload_dimensions)
            .and_then(|(width, height)| geometry_for(width, height, area, cell_pixels));
        let request = Request {
            key: key.map(str::to_owned),
            geometry,
            content,
        };

        if !self.force_replay && self.last_request.as_ref() == Some(&request) {
            return String::new();
        }

        let mut output = String::new();
        let changed = self.force_replay || self.last_request.as_ref() != Some(&request);
        if changed {
            self.hide_displayed(&mut output);
        }

        let Some(key) = key else {
            self.finish_request(request);
            return output;
        };
        let Some(geometry) = geometry else {
            self.finish_request(request);
            return output;
        };

        let needs_upload = match self.uploads.get(key) {
            Some(upload) => content.is_some_and(|content| content != upload.content),
            None => true,
        };
        let mut uploaded = false;
        if needs_upload {
            if self.uploads.contains_key(key) {
                self.remove_upload(key, true, &mut output);
            }
            let Some(image) = image else {
                self.finish_request(request);
                return output;
            };
            let Some(id) = self.allocate_id() else {
                self.finish_request(request);
                return output;
            };
            let Some(payload) = encode_upload(image, id) else {
                self.finish_request(request);
                return output;
            };
            while self.uploads.len() >= GPU_CACHE_ITEMS {
                let Some(oldest) = self.upload_order.pop_back() else {
                    break;
                };
                if let Some(upload) = self.uploads.remove(&oldest) {
                    output.push_str(&delete_image_data(upload.id));
                }
            }
            self.upload_order.push_front(key.to_owned());
            self.uploads.insert(
                key.to_owned(),
                Upload {
                    id,
                    content: content.expect("new upload has an image"),
                    width: image.width,
                    height: image.height,
                    payload,
                },
            );
            uploaded = true;
        } else {
            self.touch_upload(key);
        }

        let Some(upload) = self.uploads.get(key) else {
            self.finish_request(request);
            return output;
        };
        if uploaded {
            output.push_str(&upload.payload);
        }
        output.push_str(&place_sequence(upload.id, geometry));
        self.displayed = Some(Displayed {
            key: key.to_owned(),
            id: upload.id,
        });
        self.finish_request(request);
        output
    }

    /// Deletes every image uploaded by this `Graphics` instance.
    pub fn clear(&mut self) -> String {
        let mut ids: Vec<_> = self.uploads.values().map(|upload| upload.id).collect();
        ids.sort_unstable();
        self.uploads.clear();
        self.upload_order.clear();
        self.displayed = None;
        self.last_request = None;
        self.force_replay = false;
        ids.into_iter().map(delete_image_data).collect::<String>()
    }

    fn finish_request(&mut self, request: Request) {
        self.last_request = Some(request);
        self.force_replay = false;
    }

    fn allocate_id(&mut self) -> Option<u32> {
        let occupied: HashSet<_> = self.uploads.values().map(|upload| upload.id).collect();
        for _ in 0..=u8::MAX {
            self.next_sequence = self.next_sequence.wrapping_add(1);
            let id = self.namespace | u32::from(self.next_sequence);
            if id != 0 && !occupied.contains(&id) {
                return Some(id);
            }
        }
        None
    }

    fn touch_upload(&mut self, key: &str) {
        self.upload_order.retain(|candidate| candidate != key);
        self.upload_order.push_front(key.to_owned());
    }

    fn hide_displayed(&mut self, output: &mut String) {
        if let Some(displayed) = self.displayed.take() {
            output.push_str(&delete_placement(displayed.id));
        }
    }

    fn remove_upload(&mut self, key: &str, delete_data: bool, output: &mut String) {
        if let Some(upload) = self.uploads.remove(key)
            && delete_data
        {
            output.push_str(&delete_image_data(upload.id));
        }
        self.upload_order.retain(|candidate| candidate != key);
    }

    #[cfg(test)]
    fn cached_uploads(&self) -> usize {
        self.uploads.len()
    }

    #[cfg(test)]
    fn displayed_id(&self) -> Option<u32> {
        self.displayed.as_ref().map(|displayed| displayed.id)
    }
}

impl Default for Graphics {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&Image> for ImageContent {
    fn from(image: &Image) -> Self {
        let digest: [u8; 32] = Sha256::digest(&image.png).into();
        Self {
            digest,
            len: image.png.len(),
            width: image.width,
            height: image.height,
        }
    }
}

fn upload_dims(upload: &Upload) -> (u32, u32) {
    (upload.width, upload.height)
}

fn geometry_for(
    image_width: u32,
    image_height: u32,
    area: Rect,
    cell_pixels: (u16, u16),
) -> Option<Geometry> {
    if image_width == 0
        || image_height == 0
        || area.width == 0
        || area.height == 0
        || cell_pixels.0 == 0
        || cell_pixels.1 == 0
    {
        return None;
    }

    let max_columns = u32::from(area.width);
    let max_rows = u32::from(area.height);
    let cell_ratio = f64::from(cell_pixels.1) / f64::from(cell_pixels.0);
    let image_ratio = f64::from(image_width) / f64::from(image_height) * cell_ratio;
    let area_ratio = f64::from(max_columns) / f64::from(max_rows);
    let (columns, rows) = if image_ratio >= area_ratio {
        let rows = (f64::from(max_columns) / image_ratio).floor() as u32;
        (max_columns, rows.clamp(1, max_rows))
    } else {
        let columns = (f64::from(max_rows) * image_ratio).floor() as u32;
        (columns.clamp(1, max_columns), max_rows)
    };
    Some(Geometry {
        column: u32::from(area.x) + 1 + (max_columns - columns) / 2,
        row: u32::from(area.y) + 1 + (max_rows - rows) / 2,
        columns,
        rows,
    })
}

fn encode_upload(image: &Image, id: u32) -> Option<String> {
    if image.png.is_empty()
        || image.png.len() > MAX_GRAPHICS_PNG_BYTES
        || id == 0
        || image.width == 0
        || image.height == 0
    {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(&image.png))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if format != image::ImageFormat::Png || width != image.width || height != image.height {
        return None;
    }

    let chunks = image.png.len().div_ceil(GRAPHICS_CHUNK_BYTES);
    let mut output = String::with_capacity(image.png.len().div_ceil(3) * 4 + chunks * 48 + 64);
    for (index, chunk) in image.png.chunks(GRAPHICS_CHUNK_BYTES).enumerate() {
        let more = usize::from(index + 1 < chunks);
        if index == 0 {
            output.push_str(&format!("\x1b_Ga=t,t=d,f=100,i={id},q=2,m={more};"));
        } else {
            output.push_str(&format!("\x1b_Gm={more},q=2;"));
        }
        STANDARD.encode_string(chunk, &mut output);
        output.push_str("\x1b\\");
    }
    Some(output)
}

fn place_sequence(id: u32, geometry: Geometry) -> String {
    format!(
        "\x1b7\x1b[{};{}H{}\x1b8",
        geometry.row,
        geometry.column,
        placement(id, geometry)
    )
}

fn placement(id: u32, geometry: Geometry) -> String {
    format!(
        "\x1b_Ga=p,i={id},p=1,c={},r={},C=1,q=2,z=-1\x1b\\",
        geometry.columns, geometry.rows
    )
}

fn delete_placement(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\")
}

fn delete_image_data(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

fn env_value(name: &str) -> Option<String> {
    env::var_os(name)
        .map(|value| value.to_string_lossy().trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn artwork(color: [u8; 4]) -> Image {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 50, Rgba(color)))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        Image {
            png: png.into_inner(),
            width: 100,
            height: 50,
            palette: super::super::NEUTRAL_PALETTE,
        }
    }

    fn count_uploads(output: &str) -> usize {
        output.matches("a=t,t=d").count()
    }

    fn count_places(output: &str) -> usize {
        output.matches("a=p,").count()
    }

    fn count_placement_deletes(output: &str) -> usize {
        output.matches("d=i,").count()
    }

    #[test]
    fn static_frame_has_zero_output() {
        let image = artwork([200, 30, 20, 255]);
        let mut graphics = Graphics::new();
        let area = Rect::new(2, 3, 20, 10);

        let first = graphics.update(Some("album"), Some(&image), area, (10, 20), true);
        let second = graphics.update(Some("album"), Some(&image), area, (10, 20), true);

        assert_eq!(count_uploads(&first), 1);
        assert_eq!(count_places(&first), 1);
        assert!(first.contains("q=2"));
        assert!(first.contains("\x1b[6;3H"));
        assert!(second.is_empty());
    }

    #[test]
    fn geometry_changes_reuse_the_upload() {
        let image = artwork([20, 180, 40, 255]);
        let mut graphics = Graphics::new();
        graphics.update(
            Some("album"),
            Some(&image),
            Rect::new(0, 0, 30, 12),
            (10, 20),
            true,
        );

        let moved = graphics.update(
            Some("album"),
            Some(&image),
            Rect::new(4, 2, 20, 8),
            (10, 20),
            true,
        );
        assert_eq!(count_uploads(&moved), 0);
        assert_eq!(count_placement_deletes(&moved), 1);
        assert_eq!(count_places(&moved), 1);
    }

    #[test]
    fn hiding_only_deletes_the_placement_and_keeps_the_upload() {
        let image = artwork([30, 60, 200, 255]);
        let mut graphics = Graphics::new();
        let area = Rect::new(0, 0, 20, 10);
        graphics.update(Some("album"), Some(&image), area, (8, 16), true);

        let hidden = graphics.update(Some("album"), Some(&image), area, (8, 16), false);
        assert_eq!(count_placement_deletes(&hidden), 1);
        assert_eq!(count_uploads(&hidden), 0);
        assert_eq!(graphics.cached_uploads(), 1);
        assert!(
            graphics
                .update(Some("album"), Some(&image), area, (8, 16), false)
                .is_empty()
        );

        let shown = graphics.update(Some("album"), Some(&image), area, (8, 16), true);
        assert_eq!(count_uploads(&shown), 0);
        assert_eq!(count_places(&shown), 1);
    }

    #[test]
    fn invalidate_replaces_from_cache_without_reuploading() {
        let image = artwork([150, 80, 40, 255]);
        let mut graphics = Graphics::new();
        let area = Rect::new(1, 1, 16, 8);
        graphics.update(Some("album"), Some(&image), area, (8, 16), true);

        graphics.invalidate();
        let replay = graphics.update(Some("album"), Some(&image), area, (8, 16), true);
        assert_eq!(count_uploads(&replay), 0);
        assert_eq!(count_placement_deletes(&replay), 1);
        assert_eq!(count_places(&replay), 1);
    }

    #[test]
    fn gpu_cache_evicts_the_oldest_of_thirty_two_images() {
        let mut graphics = Graphics::new();
        let mut images = Vec::new();
        for index in 0..33 {
            images.push(artwork([index, 40, 200 - index, 255]));
        }
        let area = Rect::new(0, 0, 12, 8);

        for (index, image) in images.iter().enumerate() {
            let key = format!("album-{index}");
            let output = graphics.update(Some(&key), Some(image), area, (8, 16), true);
            assert_eq!(count_uploads(&output), 1);
        }
        assert_eq!(graphics.cached_uploads(), 32);

        let recent = graphics.update(Some("album-31"), Some(&images[31]), area, (8, 16), true);
        assert_eq!(count_uploads(&recent), 0);

        let evicted = graphics.update(Some("album-0"), Some(&images[0]), area, (8, 16), true);
        assert_eq!(count_uploads(&evicted), 1);
    }

    #[test]
    fn image_id_allocation_skips_live_ids_after_sequence_wrap() {
        let first_image = artwork([10, 20, 30, 255]);
        let second_image = artwork([40, 50, 60, 255]);
        let area = Rect::new(0, 0, 10, 10);
        let mut graphics = Graphics::new();
        graphics.update(Some("first"), Some(&first_image), area, (8, 16), true);
        let first_id = graphics.uploads["first"].id;

        graphics.next_sequence = 0;
        graphics.update(Some("second"), Some(&second_image), area, (8, 16), true);
        assert_ne!(graphics.uploads["second"].id, first_id);
    }

    #[test]
    fn recover_reuploads_only_the_displayed_image() {
        let first_image = artwork([10, 120, 200, 255]);
        let second_image = artwork([200, 80, 20, 255]);
        let area = Rect::new(0, 0, 12, 8);
        let mut graphics = Graphics::new();
        graphics.update(Some("first"), Some(&first_image), area, (8, 16), true);
        graphics.update(Some("second"), Some(&second_image), area, (8, 16), true);
        let first_id = graphics.uploads["first"].id;
        let second_id = graphics.uploads["second"].id;

        let recovered = graphics.recover();
        assert_eq!(count_uploads(&recovered), 0);
        assert_eq!(count_places(&recovered), 0);
        assert!(recovered.contains(&format!("d=I,i={second_id}")));
        assert!(!recovered.contains(&format!("i={first_id}")));
        assert_eq!(graphics.cached_uploads(), 1);
        assert!(graphics.recover().is_empty());

        let restored = graphics.update(Some("second"), Some(&second_image), area, (8, 16), true);
        assert_eq!(count_uploads(&restored), 1);
        assert_eq!(count_places(&restored), 1);
        assert_ne!(graphics.uploads["second"].id, second_id);
    }

    #[test]
    fn clear_only_deletes_ids_uploaded_by_this_instance() {
        let first_image = artwork([200, 20, 30, 255]);
        let second_image = artwork([20, 30, 200, 255]);
        let area = Rect::new(0, 0, 10, 10);
        let mut first = Graphics::new();
        let mut second = Graphics::new();
        first.update(Some("first"), Some(&first_image), area, (8, 16), true);
        second.update(Some("second"), Some(&second_image), area, (8, 16), true);

        let first_id = first.displayed_id().unwrap();
        let second_id = second.displayed_id().unwrap();
        assert_ne!(first_id, second_id);
        let cleared = first.clear();
        assert!(cleared.contains(&format!("i={first_id}")));
        assert!(!cleared.contains(&format!("i={second_id}")));
        assert!(cleared.contains("d=I"));
    }

    #[test]
    fn aspect_ratio_is_centered_and_one_based() {
        let geometry = geometry_for(100, 100, Rect::new(4, 3, 10, 10), (10, 20)).unwrap();
        assert_eq!(
            geometry,
            Geometry {
                column: 5,
                row: 6,
                columns: 10,
                rows: 5,
            }
        );
    }
}
