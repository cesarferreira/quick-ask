use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_channel::oneshot;
use gpui::http_client::{
    AsyncBody, HttpClient, Method, RedirectPolicy, Request, Response, Url, http::HeaderValue,
};
use image::{ImageDecoder as _, ImageFormat};
use std::{
    borrow::Cow,
    future::Future,
    io::{Cursor, Read as _},
    pin::Pin,
    thread,
    time::Duration,
};

type ImageResponse = Pin<Box<dyn Future<Output = Result<Response<AsyncBody>>> + Send>>;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 5;
const MAX_IMAGE_DIMENSION: u32 = 8_192;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_ANIMATION_PIXELS: u64 = 24_000_000;
const MAX_ANIMATION_FRAMES: u64 = 120;

pub struct ImageAssets;

impl gpui::AssetSource for ImageAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if is_data_uri(path) {
            let (_, bytes) = decode_data_image(path)?;
            Ok(Some(Cow::Owned(bytes)))
        } else {
            gpui_component_assets::Assets.load(path)
        }
    }

    fn list(&self, path: &str) -> Result<Vec<gpui::SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}

/// A separate, anonymous transport for images rendered in answers.
pub struct ImageHttpClient {
    client: reqwest::blocking::Client,
}

impl ImageHttpClient {
    pub fn new() -> Result<Self> {
        // Keep reqwest's blocking runtime construction outside any UI/async runtime.
        let client = thread::Builder::new()
            .name("quick-ask-image-client".into())
            .spawn(|| {
                reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(20))
                    .redirect(reqwest::redirect::Policy::none())
                    .referer(false)
                    .no_proxy()
                    .build()
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("image HTTP client initialization failed"))??;
        Ok(Self { client })
    }

    fn load(&self, uri: String, redirect_limit: u32) -> ImageResponse {
        let client = self.client.clone();
        let (sender, receiver) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("quick-ask-image".into())
            .spawn(move || {
                let _ = sender.send(load_image(&client, &uri, redirect_limit));
            });
        Box::pin(async move {
            worker.context("starting image loader")?;
            receiver.await.context("image loader stopped")?
        })
    }
}

impl HttpClient for ImageHttpClient {
    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(&self, request: Request<AsyncBody>) -> ImageResponse {
        if request.method() != Method::GET {
            return Box::pin(async { bail!("image transport only supports GET requests") });
        }
        let redirect_limit = match request.extensions().get::<RedirectPolicy>() {
            Some(RedirectPolicy::FollowAll) => MAX_REDIRECTS,
            Some(RedirectPolicy::FollowLimit(limit)) => (*limit).min(MAX_REDIRECTS),
            _ => 0,
        };
        // Deliberately copy neither request headers nor its body into this transport.
        self.load(request.uri().to_string(), redirect_limit)
    }

    fn get(&self, uri: &str, _body: AsyncBody, follow_redirects: bool) -> ImageResponse {
        // http::Uri rejects data URLs, so handle the original URI before building a request.
        self.load(
            uri.to_owned(),
            if follow_redirects { MAX_REDIRECTS } else { 0 },
        )
    }
}

fn load_image(
    client: &reqwest::blocking::Client,
    uri: &str,
    redirect_limit: u32,
) -> Result<Response<AsyncBody>> {
    if is_data_uri(uri) {
        let (mime, bytes) = decode_data_image(uri)?;
        return Ok(Response::builder()
            .status(200)
            .header("Content-Type", mime)
            .body(bytes.into())?);
    }
    let mut url = Url::parse(uri).context("invalid image URL")?;
    validate_remote_url(&url)?;

    for redirects in 0..=redirect_limit {
        let mut response = client
            .get(url.clone())
            .header("Accept", "image/*")
            .send()
            .map_err(reqwest::Error::without_url)
            .context("requesting image")?;
        let status = response.status();
        let is_redirect = matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308);
        if is_redirect && redirect_limit > 0 {
            ensure!(redirects < redirect_limit, "image redirect limit exceeded");
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("image redirect has no destination")?
                .to_str()
                .context("invalid image redirect destination")?;
            let destination = url.join(location).context("invalid image redirect URL")?;
            validate_redirect(&url, &destination)?;
            url = destination;
            continue;
        }

        let mut result = Response::builder().status(status.as_u16());
        if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) {
            result = result.header("Content-Type", content_type.as_bytes());
        }
        if !status.is_success() {
            return Ok(result.body(AsyncBody::empty())?);
        }
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_IMAGE_BYTES as u64),
            "image exceeds the 16 MiB size limit"
        );
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("reading image")?;
        ensure!(
            bytes.len() <= MAX_IMAGE_BYTES,
            "image exceeds the 16 MiB size limit"
        );
        validate_image_memory(&bytes)?;
        return Ok(result.body(bytes.into())?);
    }
    unreachable!("bounded redirect loop returns or errors on its final iteration")
}

fn validate_remote_url(url: &Url) -> Result<()> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "unsupported image URL scheme"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "image URLs cannot contain credentials"
    );
    Ok(())
}

fn validate_redirect(source: &Url, destination: &Url) -> Result<()> {
    validate_remote_url(destination)?;
    ensure!(
        source.scheme() != "https" || destination.scheme() == "https",
        "image redirect cannot downgrade HTTPS"
    );
    Ok(())
}

fn is_data_uri(uri: &str) -> bool {
    uri.get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn decode_data_image(uri: &str) -> Result<(&'static str, Vec<u8>)> {
    let (metadata, encoded) = uri[5..].split_once(',').context("invalid image data URL")?;
    let mime = match metadata.to_ascii_lowercase().as_str() {
        "image/png;base64" => "image/png",
        "image/jpeg;base64" => "image/jpeg",
        "image/webp;base64" => "image/webp",
        "image/gif;base64" => "image/gif",
        _ => bail!("unsupported image data URL format"),
    };
    ensure!(!encoded.is_empty(), "empty image data URL");
    ensure!(
        encoded.len() <= MAX_IMAGE_BYTES.div_ceil(3) * 4,
        "image exceeds the 16 MiB size limit"
    );
    let bytes = STANDARD
        .decode(encoded)
        .context("invalid base64 image data")?;
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "image exceeds the 16 MiB size limit"
    );
    validate_image_memory(&bytes)?;
    Ok((mime, bytes))
}

fn validate_image_memory(bytes: &[u8]) -> Result<()> {
    let format = image::guess_format(bytes).context("unsupported image format")?;
    match format {
        ImageFormat::Gif => {
            let mut options = gif::DecodeOptions::new();
            options.skip_frame_decoding(true);
            options.check_frame_consistency(true);
            options.set_memory_limit(gif::MemoryLimit::Bytes(
                (MAX_IMAGE_BYTES as u64)
                    .try_into()
                    .expect("nonzero image byte limit"),
            ));
            let mut decoder = options
                .read_info(Cursor::new(bytes))
                .context("invalid GIF metadata")?;
            let pixels =
                validate_dimensions(u32::from(decoder.width()), u32::from(decoder.height()))?;
            let mut frames = 0;
            // GPUI retains one full-canvas RGBA buffer for every GIF frame, including tiny subframes.
            // Skip LZW expansion here; inspect no more than the first over-budget frame header.
            while decoder
                .next_frame_info()
                .context("invalid GIF frame metadata")?
                .is_some()
            {
                frames += 1;
                validate_animation_budget(pixels, frames)?;
            }
            ensure!(frames > 0, "GIF has no frames");
        }
        ImageFormat::WebP => {
            let decoder = image_webp::WebPDecoder::new(Cursor::new(bytes))
                .context("invalid WebP metadata")?;
            let (width, height) = decoder.dimensions();
            let pixels = validate_dimensions(width, height)?;
            if decoder.is_animated() {
                validate_animation_budget(pixels, u64::from(decoder.num_frames()))?;
            }
            validate_webp_bitstreams(bytes, (width, height))?;
        }
        ImageFormat::Png | ImageFormat::Jpeg => {
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
            limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
            limits.max_alloc = Some(64 * 1024 * 1024);
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(limits);
            let decoder = reader
                .into_decoder()
                .context("image dimension limit exceeded or invalid image metadata")?;
            let (width, height) = decoder.dimensions();
            validate_dimensions(width, height)?;
            ensure!(
                decoder.total_bytes() <= 64 * 1024 * 1024,
                "image decoded byte limit exceeded"
            );
        }
        _ => bail!("unsupported image format; use PNG, JPEG, GIF, or WebP"),
    }
    Ok(())
}

fn validate_dimensions(width: u32, height: u32) -> Result<u64> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_IMAGE_DIMENSION && height <= MAX_IMAGE_DIMENSION,
        "image dimension limit exceeded"
    );
    let pixels = u64::from(width) * u64::from(height);
    ensure!(pixels <= MAX_IMAGE_PIXELS, "image pixel limit exceeded");
    Ok(pixels)
}

fn validate_animation_budget(canvas_pixels: u64, frames: u64) -> Result<()> {
    ensure!(
        frames > 0 && frames <= MAX_ANIMATION_FRAMES,
        "animation frame limit exceeded"
    );
    ensure!(
        canvas_pixels.saturating_mul(frames) <= MAX_ANIMATION_PIXELS,
        "animation pixel limit exceeded"
    );
    Ok(())
}

fn validate_webp_bitstreams(bytes: &[u8], canvas: (u32, u32)) -> Result<()> {
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
    let end = riff_size
        .checked_add(8)
        .context("invalid WebP RIFF length")?;
    ensure!(
        end == bytes.len(),
        "WebP RIFF length differs from file size"
    );
    let mut chunks = bytes.get(12..end).context("incomplete WebP RIFF data")?;
    while let Some(chunk) = next_webp_chunk(&mut chunks)? {
        match &chunk[..4] {
            b"VP8 " | b"VP8L" => validate_webp_frame_bitstream(chunk, canvas)?,
            b"ANMF" => {
                let payload_len = u32::from_le_bytes(chunk[4..8].try_into()?) as usize;
                ensure!(payload_len >= 16, "invalid WebP animation frame");
                let width = 1 + u32::from_le_bytes([chunk[14], chunk[15], chunk[16], 0]);
                let height = 1 + u32::from_le_bytes([chunk[17], chunk[18], chunk[19], 0]);
                validate_dimensions(width, height)?;
                ensure!(
                    width <= canvas.0 && height <= canvas.1,
                    "WebP frame exceeds canvas"
                );
                let mut subchunks = &chunk[24..8 + payload_len];
                while let Some(subchunk) = next_webp_chunk(&mut subchunks)? {
                    if matches!(&subchunk[..4], b"VP8 " | b"VP8L") {
                        validate_webp_frame_bitstream(subchunk, (width, height))?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn next_webp_chunk<'a>(remaining: &mut &'a [u8]) -> Result<Option<&'a [u8]>> {
    if remaining.is_empty() {
        return Ok(None);
    }
    ensure!(remaining.len() >= 8, "incomplete WebP chunk header");
    let length = u32::from_le_bytes(remaining[4..8].try_into()?) as usize;
    let total = length
        .checked_add(8 + (length % 2))
        .context("invalid WebP chunk length")?;
    let chunk = remaining.get(..total).context("incomplete WebP chunk")?;
    *remaining = &remaining[total..];
    Ok(Some(chunk))
}

fn validate_webp_frame_bitstream(chunk: &[u8], expected: (u32, u32)) -> Result<()> {
    // Extended/animated WebP metadata can claim a tiny canvas around an oversized VP8 bitstream.
    // Parse that bitstream's own header before GPUI's decoder can expand it.
    let mut standalone = Vec::with_capacity(chunk.len() + 12);
    standalone.extend_from_slice(b"RIFF");
    standalone.extend_from_slice(&((chunk.len() + 4) as u32).to_le_bytes());
    standalone.extend_from_slice(b"WEBP");
    standalone.extend_from_slice(chunk);
    let decoder = image_webp::WebPDecoder::new(Cursor::new(standalone))
        .context("invalid WebP bitstream metadata")?;
    let dimensions = decoder.dimensions();
    validate_dimensions(dimensions.0, dimensions.1)?;
    ensure!(
        dimensions == expected,
        "WebP bitstream dimensions differ from canvas"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::{future::block_on, io::AsyncReadExt as _};
    use gpui::http_client::{HttpRequestExt as _, Method, RedirectPolicy};
    use image::{AnimationDecoder as _, ImageEncoder as _};
    use std::{io::Write as _, net::TcpListener, sync::mpsc, thread, time::Duration};

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=";
    const IMAGE_LIMIT: usize = 16 * 1024 * 1024;

    fn image_bytes() -> Vec<u8> {
        STANDARD.decode(PNG_BASE64).unwrap()
    }

    fn encoded_gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut bytes, width, height, &[0, 0, 0, 255, 255, 255]).unwrap();
            for _ in 0..frames {
                encoder
                    .write_frame(&gif::Frame {
                        width: 1,
                        height: 1,
                        delay: 1,
                        buffer: Cow::Borrowed(&[0]),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        bytes
    }

    fn encoded_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut bytes,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        )
        .write_image(
            &vec![0; width as usize * height as usize * 4],
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        bytes
    }

    fn webp_chunk(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(name);
        chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        chunk.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    fn animated_webp(width: u32, height: u32, frames: usize) -> Vec<u8> {
        let mut static_webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut static_webp)
            .write_image(&[0, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut extended_header = vec![2, 0, 0, 0];
        extended_header.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        extended_header.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        let mut chunks = webp_chunk(b"VP8X", &extended_header);
        chunks.extend_from_slice(&webp_chunk(b"ANIM", &[0; 6]));
        for _ in 0..frames {
            let mut frame = vec![0; 16];
            frame[12] = 10;
            frame.extend_from_slice(&static_webp[12..]);
            chunks.extend_from_slice(&webp_chunk(b"ANMF", &frame));
        }
        let mut result = b"RIFF".to_vec();
        result.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        result.extend_from_slice(b"WEBP");
        result.extend_from_slice(&chunks);
        result
    }

    fn assert_rejected_through_all_image_paths(bytes: &[u8], mime: &str, expected: &str) {
        use gpui::AssetSource as _;
        let uri = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
        let data_error = get(&uri).err().expect("accepted oversized data image");
        assert!(data_error.to_string().contains(expected), "{data_error:#}");
        let asset_error = ImageAssets
            .load(&uri)
            .expect_err("accepted oversized embedded image");
        assert!(
            asset_error.to_string().contains(expected),
            "{asset_error:#}"
        );

        let mut reply = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
        reply.extend_from_slice(bytes);
        let (uri, _request, worker) = serve_once(reply);
        let remote_error = get(&uri).err().expect("accepted oversized remote image");
        assert!(
            remote_error.to_string().contains(expected),
            "{remote_error:#}"
        );
        worker.join().unwrap();
    }

    #[test]
    fn compressed_gif_with_excessive_frames_is_rejected_before_gpui_retains_them() {
        let gif = encoded_gif(1, 1, 121);
        assert!(gif.len() < 5_000);
        assert_rejected_through_all_image_paths(&gif, "image/gif", "frame limit");
    }

    #[test]
    fn compressed_gif_limits_the_full_canvas_pixels_gpui_keeps_for_every_frame() {
        let gif = encoded_gif(1_000, 1_000, 25);
        assert!(gif.len() < 2_000);
        assert_rejected_through_all_image_paths(&gif, "image/gif", "animation pixel limit");
    }

    #[test]
    fn static_image_dimensions_and_pixels_are_bounded_before_decode() {
        assert_rejected_through_all_image_paths(
            &encoded_png(8_193, 1),
            "image/png",
            "dimension limit",
        );
        assert_rejected_through_all_image_paths(
            &encoded_png(5_000, 4_000),
            "image/png",
            "pixel limit",
        );
    }

    #[test]
    fn ordinary_small_animations_remain_supported() {
        use gpui::AssetSource as _;
        let gif = encoded_gif(2, 2, 3);
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(&gif))
            .unwrap()
            .into_frames()
            .collect::<image::ImageResult<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 3);
        let uri = format!("data:image/gif;base64,{}", STANDARD.encode(&gif));
        assert_eq!(read_response(get(&uri).unwrap()), gif);
        assert_eq!(ImageAssets.load(&uri).unwrap().unwrap().as_ref(), gif);

        let webp = animated_webp(2, 2, 3);
        let decoder = image_webp::WebPDecoder::new(Cursor::new(&webp)).unwrap();
        assert_eq!(decoder.num_frames(), 3);
        let frames = image::codecs::webp::WebPDecoder::new(Cursor::new(&webp))
            .unwrap()
            .into_frames()
            .collect::<image::ImageResult<Vec<_>>>()
            .unwrap();
        assert_eq!(frames.len(), 3);
        let uri = format!("data:image/webp;base64,{}", STANDARD.encode(&webp));
        assert_eq!(read_response(get(&uri).unwrap()), webp);
        assert_eq!(ImageAssets.load(&uri).unwrap().unwrap().as_ref(), webp);
    }

    #[test]
    fn compressed_webp_limits_frame_count_and_aggregate_canvas_pixels() {
        for (width, height, frames, expected) in [
            (1, 1, 121, "frame limit"),
            (1_000, 1_000, 25, "animation pixel limit"),
        ] {
            let webp = animated_webp(width, height, frames);
            assert!(webp.len() < 10_000);
            let decoder = image_webp::WebPDecoder::new(Cursor::new(&webp)).unwrap();
            assert_eq!(decoder.num_frames(), frames as u32);
            assert_rejected_through_all_image_paths(&webp, "image/webp", expected);
        }
    }

    #[test]
    fn webp_cannot_hide_a_larger_bitstream_inside_a_smaller_animation_frame() {
        let mut webp = animated_webp(1, 1, 1);
        let vp8l = webp.windows(4).position(|bytes| bytes == b"VP8L").unwrap();
        // The lossless bitstream now claims 8193 pixels wide; its frame and canvas still claim 1.
        let original = u32::from_le_bytes(webp[vp8l + 9..vp8l + 13].try_into().unwrap());
        let width_header = (original & !0x3fff) | 8_192;
        webp[vp8l + 9..vp8l + 13].copy_from_slice(&width_header.to_le_bytes());
        assert_rejected_through_all_image_paths(&webp, "image/webp", "dimension limit");
    }

    #[test]
    fn remote_vectors_and_unrecognized_image_bytes_are_rejected() {
        for bytes in [
            b"not an image".as_slice(),
            b"<svg width=\"100000\" height=\"100000\"></svg>",
        ] {
            assert_rejected_through_all_image_paths(bytes, "image/png", "unsupported image format");
        }
    }

    fn read_response(mut response: Response<AsyncBody>) -> Vec<u8> {
        let mut bytes = Vec::new();
        block_on(response.body_mut().read_to_end(&mut bytes)).unwrap();
        bytes
    }

    fn get(uri: &str) -> Result<Response<AsyncBody>> {
        block_on(ImageHttpClient::new()?.get(uri, AsyncBody::empty(), true))
    }

    fn serve_once(response: Vec<u8>) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "image request never arrived"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
            }
            sender.send(String::from_utf8(request).unwrap()).unwrap();
            if let Err(error) = stream.write_all(&response) {
                assert!(
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ),
                    "writing image response: {error}"
                );
            }
        });
        (format!("http://{address}/image.png"), receiver, worker)
    }

    #[test]
    fn embedded_images_reach_gpui_as_decoded_bytes() {
        for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
            let response = get(&format!("data:{mime};base64,{PNG_BASE64}")).unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], mime);
            assert_eq!(read_response(response), image_bytes());
        }
    }

    #[test]
    fn markdown_embedded_data_images_use_the_same_decoder_and_keep_icons() {
        use gpui::AssetSource as _;

        let data = ImageAssets
            .load(&format!("data:image/png;base64,{PNG_BASE64}"))
            .unwrap()
            .unwrap();
        assert_eq!(data.as_ref(), image_bytes());
        assert!(ImageAssets.load("data:image/png;base64,%%%").is_err());
        assert!(ImageAssets.load("data:text/html;base64,aGk=").is_err());
        let oversized = STANDARD.encode(vec![0; IMAGE_LIMIT + 1]);
        assert!(
            ImageAssets
                .load(&format!("data:image/png;base64,{oversized}"))
                .is_err()
        );

        let icon = ImageAssets.load("icons/heart.svg").unwrap().unwrap();
        assert!(std::str::from_utf8(&icon).unwrap().contains("<svg"));
        assert!(
            ImageAssets
                .list("icons/")
                .unwrap()
                .iter()
                .any(|path| path.as_ref() == "icons/heart.svg")
        );
    }

    #[test]
    fn malformed_unsupported_and_oversized_data_images_fail() {
        for uri in [
            "data:image/png;base64,",
            "data:image/png;base64,%%%",
            "data:image/png;base64,aGVsbG8",
            "data:image/png,abc",
            "data:text/html;base64,PGgxPmhpPC9oMT4=",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64",
        ] {
            assert!(get(uri).is_err(), "accepted {uri}");
        }
        let oversized = STANDARD.encode(vec![0; IMAGE_LIMIT + 1]);
        assert!(get(&format!("data:image/png;base64,{oversized}")).is_err());
    }

    #[test]
    fn data_image_at_the_size_limit_is_accepted() {
        let mut png = image_bytes();
        png.resize(IMAGE_LIMIT, 0);
        let encoded = STANDARD.encode(&png);
        let bytes = read_response(get(&format!("data:image/png;base64,{encoded}")).unwrap());
        assert_eq!(bytes, png);
    }

    #[test]
    fn https_redirects_must_preserve_transport_security() {
        let secure = Url::parse("https://example.com/image.png").unwrap();
        let other_secure = Url::parse("https://images.example.com/image.png").unwrap();
        let insecure = Url::parse("http://example.com/image.png").unwrap();
        assert!(validate_redirect(&secure, &other_secure).is_ok());
        assert!(validate_redirect(&insecure, &secure).is_ok());
        assert!(validate_redirect(&secure, &insecure).is_err());
    }

    #[test]
    fn rejects_redirects_to_local_files_or_credentialed_urls() {
        for (destination, expected) in [
            ("file:///etc/hosts", "scheme"),
            ("http://user:password@127.0.0.1:1/image.png", "credentials"),
        ] {
            let (uri, _request, worker) = serve_once(
                format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
            );
            let error = get(&uri).err().unwrap();
            assert!(error.to_string().contains(expected), "{error}");
            worker.join().unwrap();
        }
    }

    #[test]
    fn default_redirects_stop_before_the_sixth_hop() {
        let mut destination = "http://127.0.0.1:1/must-not-load.png".to_owned();
        let mut requests = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..6 {
            let (uri, request, worker) = serve_once(
                format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
            );
            destination = uri;
            requests.push(request);
            workers.push(worker);
        }
        let error = get(&destination).err().unwrap();
        assert!(error.to_string().contains("redirect limit"), "{error}");
        for request in requests {
            request.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn rejects_unsupported_schemes_and_url_credentials() {
        for uri in [
            "file:///etc/hosts",
            "ftp://example.com/image.png",
            "https://user:password@example.com/image.png",
            "http://user@example.com/image.png",
            "/relative.png",
        ] {
            assert!(get(uri).is_err(), "accepted {uri}");
        }
    }

    #[test]
    fn remote_png_loads_without_forwarding_caller_credentials() {
        let png = image_bytes();
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            png.len()
        )
        .into_bytes();
        reply.extend_from_slice(&png);
        let (uri, request, worker) = serve_once(reply);
        let request_with_credentials = Request::builder()
            .uri(uri)
            .header("Authorization", "Bearer must-not-leak")
            .header("Cookie", "session=must-not-leak")
            .header("Proxy-Authorization", "Basic must-not-leak")
            .header("Referer", "https://private.example/conversation")
            .body(AsyncBody::empty())
            .unwrap();
        let response = block_on(
            ImageHttpClient::new()
                .unwrap()
                .send(request_with_credentials),
        )
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(read_response(response), png);
        let received = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(received.starts_with("GET /image.png HTTP/1.1\r\n"));
        assert!(!received.contains("must-not-leak"));
        assert!(!received.to_ascii_lowercase().contains("referer:"));
        worker.join().unwrap();
    }

    #[test]
    fn rejects_methods_other_than_get() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("http://127.0.0.1:1/image.png")
            .body(AsyncBody::from("secret"))
            .unwrap();
        let error = block_on(ImageHttpClient::new().unwrap().send(request))
            .err()
            .unwrap();
        assert!(error.to_string().contains("GET"));
    }

    #[test]
    fn remote_image_size_is_bounded_even_without_content_length() {
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 16777217\r\nConnection: close\r\n\r\n".to_vec(),
            [
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(),
                &vec![0; IMAGE_LIMIT + 1],
            ]
            .concat(),
        ] {
            let (uri, _request, worker) = serve_once(reply);
            let error = match get(&uri) {
                Err(error) => error,
                Ok(response) => panic!(
                    "accepted image with {} bytes",
                    read_response(response).len()
                ),
            };
            assert!(error.to_string().contains("16 MiB"), "{error}");
            worker.join().unwrap();
        }
    }

    #[test]
    fn follows_redirects_without_replaying_response_cookies() {
        let png = image_bytes();
        let mut reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            png.len()
        )
        .into_bytes();
        reply.extend_from_slice(&png);
        let (destination, request, image_worker) = serve_once(reply);
        let (uri, _request, redirect_worker) = serve_once(
            format!("HTTP/1.1 302 Found\r\nLocation: {destination}\r\nSet-Cookie: session=must-not-leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
        );
        assert_eq!(read_response(get(&uri).unwrap()), png);
        assert!(
            !request
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .contains("must-not-leak")
        );
        redirect_worker.join().unwrap();
        image_worker.join().unwrap();
    }

    #[test]
    fn honors_no_follow_and_redirect_limit() {
        let (uri, _request, worker) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/must-not-load.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let client = ImageHttpClient::new().unwrap();
        let response = block_on(client.get(&uri, AsyncBody::empty(), false)).unwrap();
        assert_eq!(response.status(), 302);
        worker.join().unwrap();

        let (uri, _request, worker) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/must-not-load.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let request = Request::builder()
            .uri(uri)
            .follow_redirects(RedirectPolicy::FollowLimit(0))
            .body(AsyncBody::empty())
            .unwrap();
        assert_eq!(block_on(client.send(request)).unwrap().status(), 302);
        worker.join().unwrap();
    }
}
