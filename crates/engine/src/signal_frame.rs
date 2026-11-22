//! The frame of a picture whose bytes are still on their way (SIGNAL.md, "Vorschau vor
//! Bestätigung"): the one who brings a big picture in sends its pixel size and format with the
//! command, everyone else puts an empty picture of that size and format in its place and swaps
//! the real one in when its blob is complete. The frame is a real image, so the picture keeps
//! its size, its position and its media name (`image{n}.{ext}`) through the swap.
//!
//! Each frame carries the hash of the content it stands for (a comment in the file), so two
//! pictures of the same size get two media entries, and the same picture twice gets one, as
//! the originals would.

use craft_opencore::signal::Hash;
use craft_opencore::signal::blobs::hex;

use crate::cmd::insert::base64_decode;

/// Most pixels a frame may have: it is built in memory (one byte a pixel) by everyone who
/// receives the command. A bigger picture has to be at the master whole first.
pub(crate) const MAX_FRAME_PIXELS: u64 = 50_000_000;

/// The light grey of an empty frame, and the darker grey of its border and its sign.
const FRAME_GREY: u8 = 0xE6;
const SIGN_GREY: u8 = 0x9A;

/// The image formats a frame can be made in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameFormat {
    Png,
    Jpeg,
    Gif,
}

impl FrameFormat {
    pub(crate) fn mime(self) -> &'static str {
        match self {
            FrameFormat::Png => "image/png",
            FrameFormat::Jpeg => "image/jpeg",
            FrameFormat::Gif => "image/gif",
        }
    }

    pub(crate) fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(FrameFormat::Png),
            "image/jpeg" => Some(FrameFormat::Jpeg),
            "image/gif" => Some(FrameFormat::Gif),
            _ => None,
        }
    }

    fn of(bytes: &[u8]) -> Option<Self> {
        match bytes.get(..4) {
            Some([0x89, b'P', b'N', b'G']) => Some(FrameFormat::Png),
            Some([0xFF, 0xD8, ..]) => Some(FrameFormat::Jpeg),
            Some([b'G', b'I', b'F', _]) => Some(FrameFormat::Gif),
            _ => None,
        }
    }
}

/// What a frame needs to know of the picture in `base64`: its format and pixel size. None for
/// what a frame can't stand for (a metafile, another format, too many pixels, not an image).
pub(crate) fn describe(base64: &str) -> Option<(FrameFormat, (u32, u32))> {
    let bytes = base64_decode(base64)?;
    let format = FrameFormat::of(&bytes)?;
    let (width, height) = wordcraft_render::image_size(&bytes)?;
    fits(width, height).then_some((format, (width, height)))
}

fn fits(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width).saturating_mul(u64::from(height)) <= MAX_FRAME_PIXELS
}

/// The empty picture standing for the content `hash`: `width`×`height` pixels of light grey in
/// `format`, with the hash as a comment in the file.
pub(crate) fn build(format: FrameFormat, (width, height): (u32, u32), hash: &Hash) -> Result<Vec<u8>, String> {
    if !fits(width, height) {
        return Err(format!("a frame of {width}×{height} pixels can't be made"));
    }
    let failed = |e: image::ImageError| format!("the frame of {width}×{height} pixels: {e}");
    let mut grey = image::GrayImage::from_pixel(width, height, image::Luma([FRAME_GREY]));
    draw_sign(&mut grey);
    let mut encoded = Vec::new();
    match format {
        FrameFormat::Png => {
            image::DynamicImage::ImageLuma8(grey).write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Png).map_err(failed)?;
            png_comment(&mut encoded, hash);
        }
        FrameFormat::Jpeg => {
            image::DynamicImage::ImageLuma8(grey).write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Jpeg).map_err(failed)?;
            jpeg_comment(&mut encoded, hash);
        }
        FrameFormat::Gif => {
            let rgba = image::DynamicImage::ImageLuma8(grey).into_rgba8();
            rgba.write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Gif).map_err(failed)?;
            gif_comment(&mut encoded, hash);
        }
    }
    Ok(encoded)
}

/// What tells a frame from a grey picture, without words: a border, and in the middle an arrow
/// down onto a line (something is still coming). Left out where the frame is too small for it.
fn draw_sign(frame: &mut image::GrayImage) {
    let (width, height) = frame.dimensions();
    let mut dot = |x: u32, y: u32| {
        if let Some(pixel) = frame.get_pixel_mut_checked(x, y) {
            *pixel = image::Luma([SIGN_GREY]);
        }
    };
    let shorter = width.min(height);
    if shorter < 16 {
        return;
    }
    let border = (shorter / 200).clamp(1, 4);
    for y in 0..height {
        for x in 0..width {
            if x < border || y < border || x >= width - border || y >= height - border {
                dot(x, y);
            }
        }
    }
    if shorter < 48 {
        return;
    }
    // The arrow in a square of `size` around the middle: shaft, head, and the line under it.
    let size = (shorter / 4).clamp(24, 160);
    let stroke = (size / 10).max(2);
    let (left, top) = ((width - size) / 2, (height - size) / 2);
    let middle = left + size / 2;
    let head = size / 3;
    let tip = top + size - 2 * stroke;
    for y in top..tip.saturating_sub(head / 2) {
        for x in middle.saturating_sub(stroke / 2)..=middle + stroke / 2 {
            dot(x, y);
        }
    }
    for row in 0..head {
        let reach = head - row;
        for x in middle.saturating_sub(reach)..=middle + reach {
            dot(x, tip.saturating_sub(head) + row);
        }
    }
    for y in top + size - stroke..top + size {
        for x in left..left + size {
            dot(x, y);
        }
    }
}

/// A `tEXt` chunk before the closing `IEND` chunk (the last twelve bytes of a PNG).
pub(crate) fn png_comment(png: &mut Vec<u8>, hash: &Hash) {
    const IEND_CHUNK_BYTES: usize = 12;
    let Some(insert_at) = png.len().checked_sub(IEND_CHUNK_BYTES) else { return };
    let mut type_and_data = b"tEXtComment\0".to_vec();
    type_and_data.extend_from_slice(hex(hash).as_bytes());
    let data_len = (type_and_data.len() - 4) as u32;
    let mut chunk = data_len.to_be_bytes().to_vec();
    chunk.extend_from_slice(&type_and_data);
    chunk.extend_from_slice(&crc32(&type_and_data).to_be_bytes());
    png.splice(insert_at..insert_at, chunk);
}

/// A `COM` segment after the JFIF header (`APP0`), which has to stay first.
fn jpeg_comment(jpeg: &mut Vec<u8>, hash: &Hash) {
    let mut insert_at = 2usize;
    if jpeg.get(2..4) == Some(&[0xFF, 0xE0])
        && let Some(&[high, low]) = jpeg.get(4..6)
    {
        insert_at = 4 + usize::from(u16::from_be_bytes([high, low]));
    }
    if insert_at > jpeg.len() {
        return;
    }
    let mut segment = vec![0xFF, 0xFE];
    segment.extend_from_slice(&((hash.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(hash);
    jpeg.splice(insert_at..insert_at, segment);
}

/// A comment extension before the trailer (the last byte of a GIF).
fn gif_comment(gif: &mut Vec<u8>, hash: &Hash) {
    if gif.last() != Some(&0x3B) {
        return;
    }
    let insert_at = gif.len() - 1;
    let mut extension = vec![0x21, 0xFE, hash.len() as u8];
    extension.extend_from_slice(hash);
    extension.push(0);
    gif.splice(insert_at..insert_at, extension);
}

/// The CRC of a PNG chunk (ISO 3309, as zlib's `crc32`).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
