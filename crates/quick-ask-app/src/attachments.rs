use anyhow::{Context as _, Result, ensure};
use image::{ImageDecoder as _, ImageEncoder as _, ImageFormat, RgbaImage, imageops::FilterType};
use quick_ask_core::chat::ImageAttachment;
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs,
    io::{self, Cursor, Read as _, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

pub const MAX_ATTACHMENTS: usize = 4;
const MAX_INPUT_BYTES: usize = 50 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_INPUT_DIMENSION: u32 = 12_000;
const MAX_INPUT_PIXELS: u64 = 32_000_000;
const MAX_SAVED_DIMENSION: u32 = 4_096;
const MAX_SAVED_BYTES: usize = 8 * 1024 * 1024;
static NEXT_IMPORT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct AttachmentStore {
    data_dir: PathBuf,
}

impl AttachmentStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn import_path(&self, path: &Path) -> Result<ImageAttachment> {
        let metadata = fs::symlink_metadata(path).context("opening image file")?;
        ensure!(metadata.is_file(), "choose a regular image file");
        ensure!(
            metadata.len() <= MAX_INPUT_BYTES as u64,
            "image exceeds the 50 MiB input limit"
        );
        let file = fs::File::open(path).context("opening image file")?;
        ensure!(file.metadata()?.is_file(), "choose a regular image file");
        let mut bytes = Vec::new();
        file.take((MAX_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("reading image file")?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        self.import_bytes(&name, &bytes)
    }

    pub fn import_bytes(&self, name: &str, bytes: &[u8]) -> Result<ImageAttachment> {
        ensure!(
            bytes.len() <= MAX_INPUT_BYTES,
            "image exceeds the 50 MiB input limit"
        );
        let (encoded, extension) = normalize_image(bytes)?;
        let directory = self.data_dir.join("images");
        secure_directory(&self.data_dir)?;
        secure_directory(&directory)?;
        let path = directory.join(format!("{:x}.{extension}", Sha256::digest(&encoded)));
        if path.try_exists()? {
            verify_stored_image(&path, &encoded)?;
        } else {
            let (temporary, mut file) = create_temporary(&directory)?;
            file.write_all(&encoded)
                .context("writing image attachment")?;
            file.sync_all().context("saving image attachment")?;
            match fs::hard_link(&temporary.0, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    verify_stored_image(&path, &encoded)?;
                }
                Err(error) => return Err(error).context("publishing image attachment"),
            }
            fs::remove_file(&temporary.0).context("removing temporary image attachment")?;
            fs::File::open(&directory)?
                .sync_all()
                .context("saving attachment directory")?;
        }
        Ok(ImageAttachment {
            name: display_name(name),
            path,
        })
    }
}

fn display_name(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut result = String::new();
    for character in basename.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > 200 {
            break;
        }
        result.push(character);
    }
    let result = result.trim();
    if matches!(result, "" | "." | "..") {
        "Image".into()
    } else {
        result.into()
    }
}

fn secure_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .context("creating attachment directory")?;
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "attachment storage must be a real directory"
    );
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn verify_stored_image(path: &Path, encoded: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("checking stored attachment")?;
    ensure!(
        metadata.is_file() && metadata.len() == encoded.len() as u64,
        "stored image attachment has changed"
    );
    let file = fs::File::open(path).context("opening stored attachment")?;
    let mut stored = Vec::new();
    file.take((MAX_SAVED_BYTES + 1) as u64)
        .read_to_end(&mut stored)?;
    ensure!(stored == encoded, "stored image attachment has changed");
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

struct TemporaryImage(PathBuf);

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_temporary(directory: &Path) -> Result<(TemporaryImage, fs::File)> {
    for _ in 0..16 {
        let path = directory.join(format!(
            ".import-{}-{}.tmp",
            std::process::id(),
            NEXT_IMPORT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((TemporaryImage(path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating temporary image attachment"),
        }
    }
    anyhow::bail!("could not create a unique temporary image attachment")
}

fn normalize_image(bytes: &[u8]) -> Result<(Vec<u8>, &'static str)> {
    let format =
        image::guess_format(bytes).context("use a PNG, JPEG, WebP, GIF, BMP, or TIFF image")?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png
                | ImageFormat::Jpeg
                | ImageFormat::WebP
                | ImageFormat::Gif
                | ImageFormat::Bmp
                | ImageFormat::Tiff
        ),
        "use a PNG, JPEG, WebP, GIF, BMP, or TIFF image"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_INPUT_DIMENSION);
    limits.max_image_height = Some(MAX_INPUT_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .context("invalid image metadata or image dimensions exceed the 12000 pixel limit")?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    ensure!(
        decoder.total_bytes() <= MAX_DECODED_BYTES,
        "image exceeds the 128 MiB decoded allocation limit"
    );
    match format {
        ImageFormat::Gif => validate_gif_frame(bytes)?,
        ImageFormat::WebP => validate_webp_bitstreams(bytes, (width, height))?,
        _ => {}
    }
    let orientation = decoder.orientation().context("reading image orientation")?;
    let mut image = image::DynamicImage::from_decoder(decoder).context("decoding image")?;
    image.apply_orientation(orientation);
    let mut image = image.into_rgba8();
    if image.width().max(image.height()) > MAX_SAVED_DIMENSION {
        let ratio = f64::from(MAX_SAVED_DIMENSION) / f64::from(image.width().max(image.height()));
        image = image::imageops::resize(
            &image,
            (f64::from(image.width()) * ratio).round().max(1.0) as u32,
            (f64::from(image.height()) * ratio).round().max(1.0) as u32,
            FilterType::Triangle,
        );
    }
    let opaque = image.pixels().all(|pixel| pixel.0[3] == 255);
    loop {
        if let Some(bytes) = encode_png(&image)? {
            return Ok((bytes, "png"));
        }
        if opaque {
            for quality in [90, 80, 70] {
                if let Some(bytes) = encode_jpeg(&image, quality)? {
                    return Ok((bytes, "jpg"));
                }
            }
        }
        ensure!(
            image.width() > 1 || image.height() > 1,
            "image cannot fit the 8 MiB attachment limit"
        );
        image = image::imageops::resize(
            &image,
            (image.width() * 3 / 4).max(1),
            (image.height() * 3 / 4).max(1),
            FilterType::Triangle,
        );
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_INPUT_DIMENSION && height <= MAX_INPUT_DIMENSION,
        "image dimensions exceed the 12000 pixel limit"
    );
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_INPUT_PIXELS,
        "image exceeds the 32 million pixels limit"
    );
    Ok(())
}

fn validate_gif_frame(bytes: &[u8]) -> Result<()> {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    options.check_frame_consistency(true);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        MAX_DECODED_BYTES.try_into().expect("nonzero decode limit"),
    ));
    let mut reader = options
        .read_info(Cursor::new(bytes))
        .context("invalid GIF metadata")?;
    let frame = reader
        .next_frame_info()
        .context("invalid or oversized GIF frame")?
        .context("GIF has no frames")?;
    validate_dimensions(u32::from(frame.width), u32::from(frame.height))
        .context("oversized GIF frame")
}

fn validate_webp_bitstreams(bytes: &[u8], canvas: (u32, u32)) -> Result<()> {
    let length = u32::from_le_bytes(
        bytes
            .get(4..8)
            .context("incomplete WebP header")?
            .try_into()?,
    ) as usize;
    ensure!(
        length.checked_add(8) == Some(bytes.len()),
        "WebP size differs from file length"
    );
    let mut chunks = bytes.get(12..).context("incomplete WebP header")?;
    while let Some((kind, payload)) = next_webp_chunk(&mut chunks)? {
        if kind == b"ANMF" {
            let header = payload.get(..16).context("incomplete WebP frame header")?;
            let x = read_u24(&header[..3]) * 2;
            let y = read_u24(&header[3..6]) * 2;
            let frame = (read_u24(&header[6..9]) + 1, read_u24(&header[9..12]) + 1);
            validate_dimensions(frame.0, frame.1)?;
            ensure!(
                x + frame.0 <= canvas.0 && y + frame.1 <= canvas.1,
                "WebP frame exceeds canvas"
            );
            let mut subchunks = &payload[16..];
            while let Some((kind, payload)) = next_webp_chunk(&mut subchunks)? {
                validate_webp_bitstream(kind, payload, frame)?;
            }
        } else {
            validate_webp_bitstream(kind, payload, canvas)?;
        }
    }
    Ok(())
}

fn next_webp_chunk<'a>(bytes: &mut &'a [u8]) -> Result<Option<(&'a [u8], &'a [u8])>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let header = bytes.get(..8).context("incomplete WebP chunk header")?;
    let length = u32::from_le_bytes(header[4..8].try_into()?) as usize;
    let total = length
        .checked_add(8 + length % 2)
        .context("invalid WebP chunk length")?;
    let chunk = bytes.get(..total).context("incomplete WebP chunk")?;
    *bytes = &bytes[total..];
    Ok(Some((&chunk[..4], &chunk[8..8 + length])))
}

fn read_u24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

fn validate_webp_bitstream(kind: &[u8], payload: &[u8], expected: (u32, u32)) -> Result<()> {
    let dimensions = match kind {
        b"VP8 " => {
            let header = payload
                .get(..10)
                .context("incomplete WebP bitstream header")?;
            ensure!(
                header[0] & 1 == 0 && &header[3..6] == b"\x9d\x01\x2a",
                "invalid WebP bitstream header"
            );
            (
                u32::from(u16::from_le_bytes(header[6..8].try_into()?) & 0x3fff),
                u32::from(u16::from_le_bytes(header[8..10].try_into()?) & 0x3fff),
            )
        }
        b"VP8L" => {
            let header = payload
                .get(..5)
                .context("incomplete WebP bitstream header")?;
            ensure!(header[0] == 0x2f, "invalid WebP bitstream header");
            let bits = u32::from_le_bytes(header[1..5].try_into()?);
            (1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff))
        }
        _ => return Ok(()),
    };
    validate_dimensions(dimensions.0, dimensions.1).context("oversized WebP bitstream")?;
    ensure!(
        dimensions == expected,
        "WebP bitstream dimensions differ from canvas"
    );
    Ok(())
}

fn encode_png(image: &RgbaImage) -> Result<Option<Vec<u8>>> {
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        full: false,
    };
    let result = image::codecs::png::PngEncoder::new_with_quality(
        &mut output,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    );
    if output.full {
        return Ok(None);
    }
    result.context("encoding image attachment")?;
    Ok(Some(output.bytes))
}

fn encode_jpeg(image: &RgbaImage, quality: u8) -> Result<Option<Vec<u8>>> {
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        full: false,
    };
    let result = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality)
        .encode_image(image);
    if output.full {
        return Ok(None);
    }
    result.context("encoding image attachment")?;
    Ok(Some(output.bytes))
}

struct BoundedOutput {
    bytes: Vec<u8>,
    full: bool,
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_SAVED_BYTES.saturating_sub(self.bytes.len()) {
            self.full = true;
            return Err(io::Error::other("image exceeds the 8 MiB attachment limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::{
        fs,
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "quick-ask-import-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> AttachmentStore {
            AttachmentStore::new(self.0.join("data"))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(3, 2, |x, y| {
            Rgba([
                x as u8 * 80,
                y as u8 * 120,
                200,
                if x == 0 { 0 } else { 255 },
            ])
        }));
        let image = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(image.to_rgb8())
        } else {
            image
        };
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, format).unwrap();
        encoded.into_inner()
    }

    #[test]
    fn supported_formats_become_durable_png_or_jpeg_still_images() {
        let dir = TestDir::new();
        let store = dir.store();
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            ImageFormat::Gif,
            ImageFormat::Bmp,
            ImageFormat::Tiff,
        ] {
            let attachment = store
                .import_bytes("clipboard.bin", &fixture(format))
                .unwrap();
            assert_eq!(attachment.name, "clipboard.bin");
            assert_eq!(
                attachment.path.parent(),
                Some(dir.0.join("data/images").as_path())
            );
            let bytes = fs::read(attachment.path).unwrap();
            assert!(matches!(
                image::guess_format(&bytes).unwrap(),
                ImageFormat::Png | ImageFormat::Jpeg
            ));
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (3, 2));
        }
    }

    #[test]
    fn transparent_screenshot_pixels_are_preserved() {
        let dir = TestDir::new();
        let attachment = dir
            .store()
            .import_bytes("Screenshot.png", &fixture(ImageFormat::Png))
            .unwrap();
        let image = image::open(attachment.path).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 200, 0]);
        assert_eq!(image.get_pixel(2, 1).0, [160, 120, 200, 255]);
    }

    #[test]
    fn over_budget_headers_are_rejected_before_pixel_decode() {
        let dir = TestDir::new();
        for (width, height, reason) in [(12_001u32, 1u32, "dimensions"), (8_000, 4_001, "pixels")] {
            let mut bytes = fixture(ImageFormat::Bmp);
            bytes[18..22].copy_from_slice(&width.to_le_bytes());
            bytes[22..26].copy_from_slice(&height.to_le_bytes());
            let error = dir.store().import_bytes("large.bmp", &bytes).unwrap_err();
            assert!(error.to_string().contains(reason), "{error:#}");
        }
        assert!(!dir.0.join("data/images").exists());
    }

    #[test]
    fn input_bytes_and_sparse_files_have_a_fifty_mib_read_limit() {
        let dir = TestDir::new();
        let oversized = vec![0; 50 * 1024 * 1024 + 1];
        let error = dir
            .store()
            .import_bytes("large.png", &oversized)
            .unwrap_err();
        assert!(error.to_string().contains("50 MiB"), "{error:#}");
        let path = dir.0.join("large.png");
        fs::File::create(&path)
            .unwrap()
            .set_len(50 * 1024 * 1024 + 1)
            .unwrap();
        let error = dir.store().import_path(&path).unwrap_err();
        assert!(error.to_string().contains("50 MiB"), "{error:#}");
        assert!(!dir.0.join("data/images").exists());
    }

    #[test]
    fn large_screenshots_are_resized_to_a_four_thousand_pixel_long_edge() {
        let dir = TestDir::new();
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(5_000, 100, Rgba([10, 30, 80, 220])));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let attachment = dir
            .store()
            .import_bytes("wide.png", &bytes.into_inner())
            .unwrap();
        let decoded = image::open(attachment.path).unwrap().to_rgba8();
        assert_eq!(decoded.width(), 4_096);
        assert_eq!(decoded.height(), 82);
        assert_eq!(decoded.get_pixel(100, 10).0, [10, 30, 80, 220]);
    }

    #[test]
    fn detailed_transparent_images_are_further_resized_to_fit_saved_byte_limit() {
        let dir = TestDir::new();
        let mut state = 17u32;
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(1_600, 1_600, |_, _| {
            let mut pixel = [0; 4];
            for value in &mut pixel {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *value = state as u8;
            }
            Rgba(pixel)
        }));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let attachment = dir
            .store()
            .import_bytes("noise.png", &bytes.into_inner())
            .unwrap();
        assert!(fs::metadata(&attachment.path).unwrap().len() <= 8 * 1024 * 1024);
        let decoded = image::open(attachment.path).unwrap();
        assert!(decoded.width() < 1_600 && decoded.height() < 1_600);
        assert!(decoded.color().has_alpha());
    }

    #[test]
    fn files_and_clipboard_deduplicate_normalized_content_without_mutating_imports() {
        let dir = TestDir::new();
        let source = dir.0.join("Screenshot.png");
        let bytes = fixture(ImageFormat::Png);
        fs::write(&source, &bytes).unwrap();
        let store = dir.store();
        let first = store.import_path(&source).unwrap();
        let saved = fs::read(&first.path).unwrap();
        let duplicate = store.import_bytes("Clipboard.png", &bytes).unwrap();
        assert_eq!(first.path, duplicate.path);
        assert_eq!(first.name, "Screenshot.png");
        assert_eq!(duplicate.name, "Clipboard.png");
        let mut different = image::load_from_memory(&bytes).unwrap().to_rgba8();
        different.put_pixel(2, 1, Rgba([255, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(different)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let second = store
            .import_bytes("Different.png", &encoded.into_inner())
            .unwrap();
        assert_ne!(first.path, second.path);
        fs::remove_file(source).unwrap();
        assert_eq!(fs::read(&first.path).unwrap(), saved);
        assert_eq!(fs::read_dir(dir.0.join("data/images")).unwrap().count(), 2);
    }

    #[test]
    fn display_names_are_bounded_basenames_without_control_characters() {
        let dir = TestDir::new();
        let store = dir.store();
        let bytes = fixture(ImageFormat::Png);
        let attachment = store
            .import_bytes("../../\nScreenshot\t.png", &bytes)
            .unwrap();
        assert_eq!(attachment.name, "Screenshot.png");
        let attachment = store.import_bytes("..", &bytes).unwrap();
        assert_eq!(attachment.name, "Image");
        let attachment = store.import_bytes(&"x".repeat(500), &bytes).unwrap();
        assert!(attachment.name.len() <= 200);
    }

    #[cfg(unix)]
    #[test]
    fn attachment_directories_and_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TestDir::new();
        let attachment = dir
            .store()
            .import_bytes("image.png", &fixture(ImageFormat::Png))
            .unwrap();
        for (path, mode) in [
            (dir.0.join("data"), 0o700),
            (dir.0.join("data/images"), 0o700),
            (attachment.path, 0o600),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[test]
    fn existing_content_is_never_replaced_and_failed_publish_leaves_no_partial_files() {
        let dir = TestDir::new();
        let bytes = fixture(ImageFormat::Png);
        let store = dir.store();
        let attachment = store.import_bytes("image.png", &bytes).unwrap();
        fs::write(&attachment.path, b"changed externally").unwrap();
        assert!(store.import_bytes("image.png", &bytes).is_err());
        assert_eq!(fs::read(&attachment.path).unwrap(), b"changed externally");
        fs::remove_file(&attachment.path).unwrap();
        fs::create_dir(&attachment.path).unwrap();
        assert!(store.import_bytes("image.png", &bytes).is_err());
        assert_eq!(fs::read_dir(dir.0.join("data/images")).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_storage_and_existing_attachment_paths_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        let bytes = fixture(ImageFormat::Png);
        let outside = dir.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(dir.0.join("data")).unwrap();
        symlink(&outside, dir.0.join("data/images")).unwrap();
        assert!(dir.store().import_bytes("image.png", &bytes).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(dir.0.join("data/images")).unwrap();
        let attachment = dir.store().import_bytes("image.png", &bytes).unwrap();
        fs::remove_file(&attachment.path).unwrap();
        let target = outside.join("target");
        fs::write(&target, b"leave me alone").unwrap();
        symlink(&target, &attachment.path).unwrap();
        assert!(dir.store().import_bytes("image.png", &bytes).is_err());
        assert_eq!(fs::read(target).unwrap(), b"leave me alone");
    }

    #[test]
    fn high_bit_depth_metadata_is_rejected_before_oversized_decode_allocation() {
        let dir = TestDir::new();
        let image = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            3,
            2,
            image::Rgba([100u16; 4]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Tiff).unwrap();
        let mut bytes = encoded.into_inner();
        assert_eq!(&bytes[..2], b"II");
        let offset = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let entries = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        for entry in 0..usize::from(entries) {
            let start = offset + 2 + 12 * entry;
            let tag = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
            if let Some(dimension) = match tag {
                256 => Some(5_000u32),
                257 => Some(4_000u32),
                _ => None,
            } {
                bytes[start + 8..start + 12].copy_from_slice(&dimension.to_le_bytes());
            }
        }
        let error = dir.store().import_bytes("large.tiff", &bytes).unwrap_err();
        assert!(error.to_string().contains("allocation"), "{error:#}");
        assert!(!dir.0.join("data/images").exists());
    }

    #[test]
    fn webp_bitstream_dimensions_are_checked_before_pixel_expansion() {
        let dir = TestDir::new();
        let mut chunks = Vec::new();
        chunks.extend_from_slice(b"VP8X\x0a\0\0\0\0\0\0\0\x02\0\0\x01\0\0");
        chunks.extend_from_slice(b"VP8 \x0a\0\0\0\0\0\0\x9d\x01\x2a");
        chunks.extend_from_slice(&12_001u16.to_le_bytes());
        chunks.extend_from_slice(&1u16.to_le_bytes());
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(&chunks);
        let error = dir
            .store()
            .import_bytes("hidden-large.webp", &bytes)
            .unwrap_err();
        assert!(error.to_string().contains("bitstream"), "{error:#}");
        assert!(!dir.0.join("data/images").exists());
    }

    #[test]
    fn gif_subframe_dimensions_are_checked_before_pixel_expansion() {
        let dir = TestDir::new();
        let mut bytes = fixture(ImageFormat::Gif);
        let descriptor = bytes.iter().position(|value| *value == 0x2c).unwrap();
        bytes[descriptor + 5..descriptor + 7].copy_from_slice(&12_001u16.to_le_bytes());
        let error = dir
            .store()
            .import_bytes("hidden-large.gif", &bytes)
            .unwrap_err();
        assert!(error.to_string().contains("GIF frame"), "{error:#}");
        assert!(!dir.0.join("data/images").exists());
    }

    #[test]
    fn opaque_photos_use_jpeg_to_keep_resolution_when_png_exceeds_saved_limit() {
        let dir = TestDir::new();
        let mut state = 19u32;
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(1_800, 1_800, |_, _| {
            let mut pixel = [0, 0, 0, 255];
            for value in &mut pixel[..3] {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *value = state as u8;
            }
            Rgba(pixel)
        }));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let attachment = dir
            .store()
            .import_bytes("photo.png", &bytes.into_inner())
            .unwrap();
        let saved = fs::read(&attachment.path).unwrap();
        assert_eq!(image::guess_format(&saved).unwrap(), ImageFormat::Jpeg);
        assert_eq!(attachment.path.extension().unwrap(), "jpg");
        let decoded = image::load_from_memory(&saved).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1_800, 1_800));
        assert!(saved.len() <= 8 * 1024 * 1024);
    }

    #[test]
    fn jpeg_exif_orientation_is_applied_before_metadata_is_removed() {
        let dir = TestDir::new();
        let jpeg = fixture(ImageFormat::Jpeg);
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(b"\xff\xe1");
        bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(exif);
        bytes.extend_from_slice(&jpeg[2..]);
        let attachment = dir.store().import_bytes("portrait.jpg", &bytes).unwrap();
        let decoded = image::open(attachment.path).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
    }

    fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = kind.to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if !payload.len().is_multiple_of(2) {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn animated_images_become_one_still_frame() {
        let dir = TestDir::new();
        let red = RgbaImage::from_pixel(3, 2, Rgba([255, 0, 0, 255]));
        let blue = RgbaImage::from_pixel(3, 2, Rgba([0, 0, 255, 255]));
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .encode_frame(image::Frame::new(red.clone()))
                .unwrap();
            encoder.encode_frame(image::Frame::new(blue)).unwrap();
        }
        let mut still = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(red)
            .write_to(&mut still, ImageFormat::WebP)
            .unwrap();
        let still = still.into_inner();
        let mut chunks = webp_chunk(b"VP8X", &[2, 0, 0, 0, 2, 0, 0, 1, 0, 0]);
        chunks.extend_from_slice(&webp_chunk(b"ANIM", &[0; 6]));
        let mut frame = vec![0; 16];
        frame[6] = 2;
        frame[9] = 1;
        frame[12] = 20;
        frame[15] = 2;
        frame.extend_from_slice(&still[12..]);
        chunks.extend_from_slice(&webp_chunk(b"ANMF", &frame));
        chunks.extend_from_slice(&webp_chunk(b"ANMF", &frame));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(&chunks);
        for bytes in [gif, webp] {
            let attachment = dir.store().import_bytes("animation", &bytes).unwrap();
            let bytes = fs::read(attachment.path).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Png);
            assert_eq!(
                image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgba8()
                    .get_pixel(1, 1)
                    .0,
                [255, 0, 0, 255]
            );
        }
    }

    #[test]
    fn concurrent_duplicate_imports_publish_one_complete_file() {
        let dir = TestDir::new();
        let store = dir.store();
        let bytes = fixture(ImageFormat::Png);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let imports = (0..8)
            .map(|_| {
                let (store, bytes, barrier) = (store.clone(), bytes.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    store.import_bytes("clipboard", &bytes).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let paths = imports
            .into_iter()
            .map(|task| task.join().unwrap().path)
            .collect::<Vec<_>>();
        assert!(paths.iter().all(|path| *path == paths[0]));
        assert!(image::open(&paths[0]).is_ok());
        assert_eq!(fs::read_dir(dir.0.join("data/images")).unwrap().count(), 1);
    }

    #[test]
    fn unsupported_and_invalid_inputs_never_create_attachment_storage() {
        let dir = TestDir::new();
        for bytes in [
            b"".as_slice(),
            b"broken image",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
        ] {
            assert!(dir.store().import_bytes("image", bytes).is_err());
        }
        assert!(dir.store().import_path(&dir.0).is_err());
        assert!(!dir.0.join("data").exists());
    }
}
