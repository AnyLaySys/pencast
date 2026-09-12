use std::sync::{Arc, Mutex};

use jpeg_decoder::{Decoder, PixelFormat};

pub(crate) const FPS: u32 = 60;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const FRAME_FLAG_JPEG: u32 = 1;

pub(crate) struct Frames {
    pub(crate) buffers: [Vec<u8>; 2],
    pub(crate) front: usize,
    pub(crate) sequence: u64,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Frames {
    pub(crate) fn new() -> Self {
        Self {
            buffers: [Vec::new(), Vec::new()],
            front: 0,
            sequence: 0,
            width: 0,
            height: 0,
        }
    }
}

pub(crate) struct Config {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) payload: usize,
}

impl Config {
    pub(crate) fn parse(header: &[u8; 48]) -> Result<Self, String> {
        if &header[..8] != b"CMCONFIG" {
            return Err(String::from("Invalid stream configuration"));
        }
        let width = number(header, 8) as usize;
        let height = number(header, 12) as usize;
        let pitch = number(header, 16) as usize;
        let format = number(header, 20);
        let payload = number(header, 40) as usize;
        let flags = number(header, 44);
        let expected = width
            .checked_mul(height)
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| String::from("Unsupported stream configuration"))?;
        if width == 0
            || height == 0
            || pitch != width * 4
            || format != 0x3432_5241
            || payload != expected
            || payload > MAX_FRAME_BYTES
            || flags != FRAME_FLAG_JPEG
        {
            return Err(String::from("Unsupported stream configuration"));
        }
        Ok(Self {
            width,
            height,
            payload,
        })
    }
}

fn number(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([
        bytes[index],
        bytes[index + 1],
        bytes[index + 2],
        bytes[index + 3],
    ])
}

pub(crate) fn packet_size(header: &[u8; 16], maximum: usize) -> Result<usize, String> {
    let size = number(header, 8) as usize;
    if &header[..8] != b"CMJPEG01" || size == 0 || size > maximum {
        return Err(String::from("Invalid JPEG frame"));
    }
    Ok(size)
}

pub(crate) fn decode_jpeg(encoded: &[u8], config: &Config, raw: &mut [u8]) -> Result<(), String> {
    let mut decoder = Decoder::new(encoded);
    decoder.set_max_decoding_buffer_size(raw.len());
    let pixels = decoder
        .decode()
        .map_err(|error| format!("Invalid JPEG frame: {error}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| String::from("Invalid JPEG frame"))?;
    let expected = config
        .width
        .checked_mul(config.height)
        .and_then(|value| value.checked_mul(3))
        .ok_or_else(|| String::from("Invalid JPEG frame"))?;
    if info.width as usize != config.width
        || info.height as usize != config.height
        || info.pixel_format != PixelFormat::RGB24
        || pixels.len() != expected
    {
        return Err(String::from("Invalid JPEG frame"));
    }
    for (source, destination) in pixels
        .as_chunks::<3>()
        .0
        .iter()
        .zip(raw.as_chunks_mut::<4>().0)
    {
        destination.copy_from_slice(&[source[0], source[1], source[2], 0xff]);
    }
    Ok(())
}

pub(crate) fn publish(frames: &Arc<Mutex<Frames>>, config: &Config, raw: &[u8]) {
    let mut frames = frames.lock().unwrap();
    let back = 1 - frames.front;
    let destination = &mut frames.buffers[back];
    if destination.len() != raw.len() {
        destination.resize(raw.len(), 0);
    }
    destination.copy_from_slice(raw);
    frames.front = back;
    frames.sequence = frames.sequence.wrapping_add(1);
    frames.width = config.width;
    frames.height = config.height;
}
