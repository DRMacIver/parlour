//! Image uploads (DESIGN.md §9): checking, re-encoding and storing images.
//!
//! Every image is decoded and encoded again, which drops whatever came
//! with the pixels: EXIF (including locations), comments, colour profiles
//! and text chunks. A JPEG's orientation is applied first, so photos stay
//! the right way up. Images over [`MAX_UPLOAD_BYTES`] or
//! [`MAX_DIMENSION`] pixels on a side are refused; avatars are cropped
//! square and scaled to [`AVATAR_SIZE`].
//!
//! Files are stored once each, named by the SHA-256 of their bytes, under
//! the configured directory (`<dir>/<first two hex digits>/<hash>.<ext>`).
//! Who may fetch one is decided in [`crate::content::uploads`].

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{
    AnimationDecoder, DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits,
};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

/// The largest file accepted.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// The widest or tallest image accepted.
pub const MAX_DIMENSION: u32 = 8000;
/// The most pixels in one image (or one frame of an animation).
pub const MAX_PIXELS: u64 = 40_000_000;
/// The most frames in an animated GIF.
pub const MAX_FRAMES: usize = 300;
/// The most pixels in all the frames of an animated GIF together, counting
/// each frame as the whole canvas, which is what it decodes to: a tiny
/// file can have hundreds of one-pixel frames on a big canvas. Frames are
/// re-encoded one at a time, so this bounds the time an animation takes,
/// and the memory it takes is a few frames' worth.
pub const MAX_ANIMATION_PIXELS: u64 = MAX_PIXELS;
/// The biggest re-encoded animation.
pub const MAX_ANIMATION_BYTES: usize = 2 * MAX_UPLOAD_BYTES;
/// Avatars are this many pixels square.
pub const AVATAR_SIZE: u32 = 256;
const JPEG_QUALITY: u8 = 85;

/// What an image is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Post,
    Avatar,
}

/// An image ready to store.
#[derive(Clone, Debug)]
pub struct Processed {
    pub bytes: Vec<u8>,
    /// Lowercase hex SHA-256 of `bytes`.
    pub sha256: String,
    pub ext: &'static str,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

impl Processed {
    /// Where the image is served.
    pub fn url(&self) -> String {
        url(&self.sha256, self.ext)
    }
}

pub fn url(sha256: &str, ext: &str) -> String {
    format!("/uploads/{sha256}.{ext}")
}

/// Why an image was refused, as said to the member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    TooBig,
    NotAnImage,
    TooManyPixels,
    /// Too many frames, or too many pixels in all of them.
    AnimationTooLong,
    Unreadable,
    /// Staff removed this image.
    Removed,
    /// Too many other images are being processed.
    Busy,
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::TooBig => format!(
                "That file is too big: images can be up to {} MB.",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            ),
            Refusal::NotAnImage => {
                "That isn’t an image we can show: upload a PNG, JPEG, GIF or WebP file.".to_owned()
            }
            Refusal::TooManyPixels => format!(
                "That image is too large: images can be up to {} pixels wide and tall.",
                crate::format::count(i64::from(MAX_DIMENSION))
            ),
            Refusal::AnimationTooLong => format!(
                "That animation is too long: animations can have up to {MAX_FRAMES} frames, and {} million pixels in all their frames together.",
                MAX_ANIMATION_PIXELS / 1_000_000
            ),
            Refusal::Unreadable => {
                "That image seems to be damaged, so it can’t be read.".to_owned()
            }
            Refusal::Busy => {
                "The forum is busy with other images just now. Try again in a minute.".to_owned()
            }
            Refusal::Removed => {
                "That image was removed by the moderators, so it can’t be uploaded again."
                    .to_owned()
            }
        }
    }
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    // Decoders may need a few bytes per pixel, and some slack.
    limits.max_alloc = Some(MAX_PIXELS * 4 + 64 * 1024 * 1024);
    limits
}

/// Checks and re-encodes an uploaded image. It takes a while for big
/// images, so call it off the async workers ([`process_blocking`] is the
/// async wrapper).
pub fn process(data: &[u8], purpose: Purpose) -> Result<Processed, Refusal> {
    if data.len() > MAX_UPLOAD_BYTES {
        return Err(Refusal::TooBig);
    }
    let format = image::guess_format(data).map_err(|_| Refusal::NotAnImage)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(Refusal::NotAnImage);
    }
    // Check the size the header claims before decoding anything.
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    reader.limits(limits());
    let (width, height) = reader.into_dimensions().map_err(|_| Refusal::Unreadable)?;
    if width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(Refusal::TooManyPixels);
    }
    if format == ImageFormat::Gif && purpose == Purpose::Post {
        return reencode_gif(data);
    }
    let image = decode(data, format)?;
    let image = match purpose {
        Purpose::Post => image,
        Purpose::Avatar => {
            let side = image.width().min(image.height());
            let square = image.crop_imm(
                (image.width() - side) / 2,
                (image.height() - side) / 2,
                side,
                side,
            );
            square.resize_exact(AVATAR_SIZE, AVATAR_SIZE, FilterType::Lanczos3)
        }
    };
    // Photographs stay JPEG; anything that may be transparent, or was
    // drawn rather than photographed, becomes PNG.
    let as_jpeg =
        format == ImageFormat::Jpeg || (!image.color().has_alpha() && format == ImageFormat::WebP);
    let (width, height) = (image.width(), image.height());
    let mut bytes = Vec::new();
    let (ext, mime) = if as_jpeg {
        let rgb = image.to_rgb8();
        JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
            .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
            .map_err(|_| Refusal::Unreadable)?;
        ("jpg", "image/jpeg")
    } else {
        let (pixels, color) = if image.color().has_alpha() {
            (image.to_rgba8().into_raw(), ExtendedColorType::Rgba8)
        } else {
            (image.to_rgb8().into_raw(), ExtendedColorType::Rgb8)
        };
        PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, height, color)
            .map_err(|_| Refusal::Unreadable)?;
        ("png", "image/png")
    };
    Ok(finish(bytes, ext, mime, width, height))
}

/// Decodes a still image, turning it the way its EXIF orientation says.
fn decode(data: &[u8], format: ImageFormat) -> Result<DynamicImage, Refusal> {
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    reader.limits(limits());
    let mut decoder = reader.into_decoder().map_err(|_| Refusal::Unreadable)?;
    let orientation = decoder.orientation().map_err(|_| Refusal::Unreadable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| Refusal::Unreadable)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Re-encodes every frame of a GIF, keeping its animation and nothing
/// else. Each frame is encoded as soon as it is decoded, so only a few
/// frames are ever in memory.
fn reencode_gif(data: &[u8]) -> Result<Processed, Refusal> {
    let mut decoder = GifDecoder::new(Cursor::new(data)).map_err(|_| Refusal::Unreadable)?;
    decoder
        .set_limits(limits())
        .map_err(|_| Refusal::TooManyPixels)?;
    let (width, height) = decoder.dimensions();
    let canvas = u64::from(width) * u64::from(height);
    let mut out = Limited {
        bytes: Vec::new(),
        overflowed: false,
    };
    let mut frames = 0u64;
    let encoded = {
        let mut encoder = GifEncoder::new(&mut out);
        let encode = || -> Result<(), Refusal> {
            encoder
                .set_repeat(Repeat::Infinite)
                .map_err(|_| Refusal::Unreadable)?;
            for frame in decoder.into_frames() {
                frames += 1;
                if frames > MAX_FRAMES as u64 || frames * canvas > MAX_ANIMATION_PIXELS {
                    return Err(Refusal::AnimationTooLong);
                }
                let frame = frame.map_err(|_| Refusal::Unreadable)?;
                encoder
                    .encode_frame(frame)
                    .map_err(|_| Refusal::Unreadable)?;
            }
            Ok(())
        };
        encode()
    };
    if out.overflowed {
        return Err(Refusal::TooBig);
    }
    encoded?;
    if frames == 0 {
        return Err(Refusal::Unreadable);
    }
    Ok(finish(out.bytes, "gif", "image/gif", width, height))
}

/// A buffer that refuses to grow past [`MAX_ANIMATION_BYTES`].
struct Limited {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl std::io::Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() + buf.len() > MAX_ANIMATION_BYTES {
            self.overflowed = true;
            return Err(std::io::Error::other("the animation is too big"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn finish(
    bytes: Vec<u8>,
    ext: &'static str,
    mime: &'static str,
    width: u32,
    height: u32,
) -> Processed {
    let sha256 = hex(&Sha256::digest(&bytes));
    Processed {
        bytes,
        sha256,
        ext,
        mime,
        width,
        height,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How many images are processed at once. Each can take a few hundred
/// megabytes for a moment (a decoded frame of the largest image is
/// [`MAX_PIXELS`] × 4 bytes), so on one small box they take turns.
pub const MAX_PROCESSING_AT_ONCE: usize = 2;
/// How long an upload waits for its turn before it is refused.
const PROCESSING_WAIT: Duration = Duration::from_secs(30);

static PROCESSING: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_PROCESSING_AT_ONCE)));

/// [`process`], off the async workers, taking turns with other uploads.
pub async fn process_blocking(data: Vec<u8>, purpose: Purpose) -> Result<Processed, Refusal> {
    process_in_turn(&PROCESSING, PROCESSING_WAIT, data, purpose).await
}

/// [`process`], off the async workers, once one of `turns`' permits is
/// free: refused as [`Refusal::Busy`] if none is within `wait`. The permit
/// is held until processing ends, even if the caller stops waiting.
async fn process_in_turn(
    turns: &Arc<Semaphore>,
    wait: Duration,
    data: Vec<u8>,
    purpose: Purpose,
) -> Result<Processed, Refusal> {
    let Ok(Ok(permit)) = tokio::time::timeout(wait, turns.clone().acquire_owned()).await else {
        return Err(Refusal::Busy);
    };
    tokio::task::spawn_blocking(move || {
        let processed = process(&data, purpose);
        drop(permit);
        processed
    })
    .await
    .unwrap_or(Err(Refusal::Unreadable))
}

/// Whether `name` (from a URL) is a stored image's file name: a SHA-256
/// in lowercase hex and one of our extensions. Gives the hash and
/// extension.
pub fn parse_name(name: &str) -> Option<(&str, &str)> {
    let (hash, ext) = name.split_once('.')?;
    let ok = hash.len() == 64
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && matches!(ext, "png" | "jpg" | "gif");
    ok.then_some((hash, ext))
}

pub fn mime_of(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        _ => "image/gif",
    }
}

/// Where a stored image's file is.
pub fn path(dir: &Path, sha256: &str, ext: &str) -> PathBuf {
    dir.join(&sha256[..2]).join(format!("{sha256}.{ext}"))
}

/// Writes an image's file, unless it is there already. The file appears
/// whole or not at all: it is written to a temporary name first.
pub async fn store(dir: &Path, image: &Processed) -> std::io::Result<()> {
    let target = path(dir, &image.sha256, image.ext);
    if tokio::fs::try_exists(&target).await? {
        return Ok(());
    }
    let parent = target.parent().expect("files are in a subdirectory");
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        image.sha256,
        crate::tokens::new_token()
    ));
    tokio::fs::write(&temporary, &image.bytes).await?;
    tokio::fs::rename(&temporary, &target).await
}

/// Deletes an image's file, if it is there.
pub async fn remove(dir: &Path, sha256: &str, ext: &str) -> std::io::Result<()> {
    match tokio::fs::remove_file(path(dir, sha256, ext)).await {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::new(width, height)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn names() {
        let hash = "a".repeat(64);
        assert_eq!(
            parse_name(&format!("{hash}.png")),
            Some((hash.as_str(), "png"))
        );
        assert_eq!(parse_name(&format!("{hash}.PNG")), None);
        assert_eq!(parse_name(&format!("{}.png", "A".repeat(64))), None);
        assert_eq!(parse_name(&format!("{hash}.svg")), None);
        assert_eq!(parse_name(&format!("{hash}.png.png")), None);
        assert_eq!(parse_name("../../etc/passwd"), None);
    }

    #[test]
    fn avatars_are_square() {
        let avatar = process(&png(300, 100), Purpose::Avatar).unwrap();
        assert_eq!((avatar.width, avatar.height), (AVATAR_SIZE, AVATAR_SIZE));
        let decoded = image::load_from_memory(&avatar.bytes).unwrap();
        assert_eq!(decoded.width(), AVATAR_SIZE);
    }

    #[test]
    fn animated_gifs_keep_their_frames() {
        let mut data = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut data);
            let frames = (0..3).map(|i| {
                image::Frame::new(image::RgbaImage::from_pixel(
                    4,
                    4,
                    image::Rgba([i * 80, 0, 0, 255]),
                ))
            });
            encoder.encode_frames(frames).unwrap();
        }
        let gif = process(&data, Purpose::Post).unwrap();
        assert_eq!(gif.ext, "gif");
        let frames = GifDecoder::new(Cursor::new(&gif.bytes))
            .unwrap()
            .into_frames()
            .count();
        assert_eq!(frames, 3);
    }

    /// A GIF with many frames is refused, however small each frame is:
    /// each one decodes to the whole canvas.
    #[test]
    fn long_animations_are_refused() {
        let frame = |i: u8| {
            image::Frame::from_parts(
                image::RgbaImage::from_pixel(1, 1, image::Rgba([i, 0, 0, 255])),
                i.into(),
                0,
                image::Delay::from_numer_denom_ms(100, 1),
            )
        };
        let encode = |count: u8| {
            let mut data = Vec::new();
            {
                let mut encoder = GifEncoder::new(&mut data);
                // The first frame fills the canvas, which the rest share.
                encoder
                    .encode_frame(image::Frame::new(image::RgbaImage::new(1000, 1000)))
                    .unwrap();
                encoder.encode_frames((0..count).map(frame)).unwrap();
            }
            data
        };
        // 40 frames of a million pixels each is the most.
        assert!(process(&encode(39), Purpose::Post).is_ok());
        assert_eq!(
            process(&encode(40), Purpose::Post).unwrap_err(),
            Refusal::AnimationTooLong
        );
    }

    /// Images are processed a few at a time: one that can't get a turn
    /// in time is refused, and isn't processed.
    #[tokio::test]
    async fn images_take_turns() {
        let turns = Arc::new(Semaphore::new(1));
        let held = turns.clone().acquire_owned().await.unwrap();
        let wait = Duration::from_millis(50);
        assert_eq!(
            process_in_turn(&turns, wait, png(2, 2), Purpose::Post)
                .await
                .unwrap_err(),
            Refusal::Busy
        );
        drop(held);
        assert!(
            process_in_turn(&turns, wait, png(2, 2), Purpose::Post)
                .await
                .is_ok()
        );
        assert_eq!(turns.available_permits(), 1);
    }

    /// Arbitrary bytes never get through unless they are an image we can
    /// read, and whatever gets through is an image of the size reported,
    /// named by its hash.
    #[hegel::test(test_cases = 300)]
    fn only_images_get_through(tc: TestCase) {
        let mut data = if tc.draw(gs::booleans()) {
            png(
                tc.draw(gs::integers::<u32>().min_value(1).max_value(40)),
                tc.draw(gs::integers::<u32>().min_value(1).max_value(40)),
            )
        } else {
            Vec::new()
        };
        // Damage it, or replace it, at random.
        let noise = tc.draw(gs::binary().max_size(64));
        if !noise.is_empty() && tc.draw(gs::booleans()) {
            let at = tc.draw(gs::integers::<usize>().max_value(data.len()));
            data.splice(at..at, noise);
        }
        if let Ok(image) = process(&data, Purpose::Post) {
            let decoded = image::load_from_memory(&image.bytes).unwrap();
            assert_eq!(
                (decoded.width(), decoded.height()),
                (image.width, image.height)
            );
            assert_eq!(image.sha256, hex(&Sha256::digest(&image.bytes)));
            assert!(parse_name(&format!("{}.{}", image.sha256, image.ext)).is_some());
        }
    }
}
