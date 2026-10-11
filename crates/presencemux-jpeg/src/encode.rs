//! The entropy encoder with the standard Huffman tables, and the output
//! frame.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the codes, values and table entries of JPEG have fixed bit widths"
)]
#![expect(
    clippy::inline_always,
    reason = "the inner loops of the decoder and the encoder must inline"
)]

use std::fmt;

use crate::decode::{Coefficients, SpanView};
use crate::header::{Component, Frame};
use crate::requant::{self, Requant};

/// The number of DC codes of each length for luminance (ITU T.81, Annex K.3).
const DC_LUMA_COUNTS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_COUNTS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_COUNTS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_COUNTS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// The largest number of bytes of one block, with room for the last store.
const BLOCK_BYTES: usize = 256;

/// The Huffman code and code length of each symbol.
#[derive(Debug)]
struct Codes {
    code: [u32; 256],
    length: [u32; 256],
}

impl Codes {
    fn new(counts: &[u8; 16], values: &[u8]) -> Self {
        let mut codes = Self {
            code: [0; 256],
            length: [0; 256],
        };
        let mut symbols = values.iter();
        let mut code = 0;
        for (length, &n) in (1..=16).zip(counts) {
            for &symbol in symbols.by_ref().take(usize::from(n)) {
                codes.code[usize::from(symbol)] = code;
                codes.length[usize::from(symbol)] = length;
                code += 1;
            }
            code <<= 1;
        }
        codes
    }
}

/// The JPEG encoder at one quality.
///
/// Use one `Encoder` for all frames. `prepare` builds the requantization again
/// only when the quantization tables of a frame change.
pub struct Encoder {
    quality: u8,
    /// The luminance and chrominance tables, in zigzag order.
    target: [[u16; 64]; 2],
    dc: [Codes; 2],
    ac: [Codes; 2],
    /// The source table of each component in scan order, for which
    /// `requant` holds the requantization.
    sources: Vec<[u16; 64]>,
    requant: Vec<Requant>,
}

impl fmt::Debug for Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("quality", &self.quality)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// Creates an encoder with the libjpeg tables of `quality`. Clamps the
    /// quality to the range 1 to 100.
    pub fn new(quality: u8) -> Self {
        let quality = quality.clamp(1, 100);
        Self {
            quality,
            target: requant::tables(quality),
            dc: [
                Codes::new(&DC_LUMA_COUNTS, &DC_VALUES),
                Codes::new(&DC_CHROMA_COUNTS, &DC_VALUES),
            ],
            ac: [
                Codes::new(&AC_LUMA_COUNTS, &AC_LUMA_VALUES),
                Codes::new(&AC_CHROMA_COUNTS, &AC_CHROMA_VALUES),
            ],
            sources: Vec::new(),
            requant: Vec::new(),
        }
    }

    /// Returns the quality.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Changes the quality. Clamps the quality to the range 1 to 100. The next
    /// `prepare` builds the requantization again.
    pub fn set_quality(&mut self, quality: u8) {
        let quality = quality.clamp(1, 100);
        if quality != self.quality {
            self.quality = quality;
            self.target = requant::tables(quality);
            self.sources.clear();
        }
    }

    /// Prepares the requantization of `frame`. The first component gets the
    /// luminance table, and the others the chrominance table.
    pub fn prepare(&mut self, frame: &Frame<'_>) -> Prepared<'_> {
        let sources: Vec<[u16; 64]> = frame
            .components
            .iter()
            .map(|c| frame.quant[usize::from(c.quant)].unwrap_or([1; 64]))
            .collect();
        if sources != self.sources {
            self.requant = (0..)
                .zip(&sources)
                .map(|(i, source)| Requant::new(source, &self.target[table(i)]))
                .collect();
            self.sources = sources;
        }
        Prepared {
            encoder: self,
            width: frame.width,
            height: frame.height,
            components: frame.components.clone(),
        }
    }
}

/// Returns the table of the component with `index` in the scan. The first
/// component gets luminance, and the others get chrominance.
fn table(index: usize) -> usize {
    usize::from(index > 0)
}

/// An encoder, prepared for one frame.
#[derive(Debug)]
pub struct Prepared<'e> {
    encoder: &'e Encoder,
    width: u16,
    height: u16,
    components: Vec<Component>,
}

/// The entropy-coded bits of one part, without byte stuffing. Keep it across
/// frames to reuse its memory.
#[derive(Debug, Default)]
pub struct Encoded {
    bytes: Vec<u8>,
    bits: usize,
}

impl Encoded {
    /// Creates an empty `Encoded`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Prepared<'_> {
    /// Requantizes and encodes the blocks of part `index` of `coefficients`.
    ///
    /// # Panics
    ///
    /// Panics if `coefficients` comes from a frame with more components than
    /// the frame of `self`.
    pub fn encode(&self, coefficients: &Coefficients<'_>, index: usize, out: &mut Encoded) {
        assert!(
            coefficients
                .layout()
                .iter()
                .all(|&c| c < self.components.len()),
            "the coefficients come from another frame"
        );
        let mut writer = Writer::new(std::mem::take(&mut out.bytes), 0);
        if let Some(span) = coefficients.span(index) {
            let mut predictors = [0i32; 4];
            for (component, predictor) in predictors
                .iter_mut()
                .enumerate()
                .take(self.components.len())
            {
                if let Some(dc) = coefficients.dc_before(span.first, component) {
                    *predictor = i32::from(self.encoder.requant[component].one(0, dc));
                }
            }
            self.encode_blocks(&span, coefficients.layout(), predictors, &mut writer);
        }
        (out.bytes, out.bits) = writer.finish();
    }

    /// Requantizes and encodes the blocks of `span`, from the DC predictors
    /// `predictors`.
    fn encode_blocks(
        &self,
        span: &SpanView<'_>,
        layout: &[usize],
        mut predictors: [i32; 4],
        writer: &mut Writer,
    ) {
        let encoder = self.encoder;
        let mut phase = span.first % layout.len();
        let mut block = [0i16; 64];
        for source in span.blocks {
            let component = layout[phase];
            let offset = span.offset[component] as i16;
            let nonzero = encoder.requant[component].apply(source, offset, &mut block);
            writer.reserve(BLOCK_BYTES);
            let t = table(component);
            encode_block(
                writer,
                &block,
                nonzero,
                predictors[component],
                &encoder.dc[t],
                &encoder.ac[t],
            );
            predictors[component] = i32::from(block[0]);
            phase += 1;
            if phase == layout.len() {
                phase = 0;
            }
        }
    }

    /// Writes the JPEG frame with the encoded `parts`. It replaces the
    /// contents of `out`.
    pub fn write(&self, parts: &[Encoded], out: &mut Vec<u8>) {
        out.clear();
        self.write_headers(out);
        let start = out.len();
        let mut writer = Writer::new(std::mem::take(out), start);
        for part in parts {
            writer.reserve(part.bytes.len());
            let words = part.bits / 32;
            for word in part.bytes.as_chunks::<4>().0.iter().take(words) {
                writer.put(u64::from(u32::from_be_bytes(*word)), 32);
            }
            let mut rest = part.bits % 32;
            for &byte in &part.bytes[4 * words..] {
                if rest == 0 {
                    break;
                }
                let n = rest.min(8);
                writer.put(u64::from(byte >> (8 - n)), n as u32);
                rest -= n;
            }
        }
        // The last byte ends with 1 bits.
        let pad = (8 - writer.bit_len() % 8) % 8;
        if pad > 0 {
            writer.reserve(1);
            writer.put((1 << pad) - 1, pad as u32);
        }
        let (mut bytes, _) = writer.finish();
        stuff(&mut bytes, start);
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        *out = bytes;
    }

    /// Writes SOI, DQT, SOF0, DHT and SOS.
    fn write_headers(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[0xFF, 0xD8]);
        for (id, target) in (0..).zip(&self.encoder.target) {
            out.extend_from_slice(&[0xFF, 0xDB, 0, 67, id]);
            out.extend(target.iter().map(|&q| q as u8));
        }
        let count = self.components.len();
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&((8 + 3 * count) as u16).to_be_bytes());
        out.push(8);
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.push(count as u8);
        for (i, c) in self.components.iter().enumerate() {
            let sampling = if count == 1 { 0x11 } else { c.h << 4 | c.v };
            out.extend_from_slice(&[c.id, sampling, table(i) as u8]);
        }
        for (class_id, counts, values) in [
            (0x00, &DC_LUMA_COUNTS, &DC_VALUES[..]),
            (0x10, &AC_LUMA_COUNTS, &AC_LUMA_VALUES[..]),
            (0x01, &DC_CHROMA_COUNTS, &DC_VALUES[..]),
            (0x11, &AC_CHROMA_COUNTS, &AC_CHROMA_VALUES[..]),
        ] {
            out.extend_from_slice(&[0xFF, 0xC4]);
            out.extend_from_slice(&((3 + 16 + values.len()) as u16).to_be_bytes());
            out.push(class_id);
            out.extend_from_slice(counts);
            out.extend_from_slice(values);
        }
        out.extend_from_slice(&[0xFF, 0xDA]);
        out.extend_from_slice(&((6 + 2 * count) as u16).to_be_bytes());
        out.push(count as u8);
        for (i, c) in self.components.iter().enumerate() {
            let t = table(i) as u8;
            out.extend_from_slice(&[c.id, t << 4 | t]);
        }
        out.extend_from_slice(&[0, 63, 0]);
    }
}

/// Encodes one requantized block. It writes the DC difference to `predictor`,
/// then the AC run and size symbols. Bit 63 - k of `nonzero` is set when coefficient k
/// is not zero.
#[inline(always)]
fn encode_block(
    writer: &mut Writer,
    block: &[i16; 64],
    nonzero: u64,
    predictor: i32,
    dc: &Codes,
    ac: &Codes,
) {
    let (size, bits) = value_bits(i32::from(block[0]) - predictor);
    writer.put(
        u64::from(dc.code[size]) << size | u64::from(bits),
        dc.length[size] + size as u32,
    );

    let mut nonzero = nonzero & !(1 << 63);
    let mut last = 0;
    while nonzero != 0 {
        let k = nonzero.leading_zeros();
        let mut run = k - last - 1;
        while run >= 16 {
            writer.put(u64::from(ac.code[0xF0]), ac.length[0xF0]);
            run -= 16;
        }
        let (size, bits) = value_bits(i32::from(block[k as usize]));
        let symbol = (run as usize) << 4 | size;
        writer.put(
            u64::from(ac.code[symbol]) << size | u64::from(bits),
            ac.length[symbol] + size as u32,
        );
        nonzero ^= (1 << 63) >> k;
        last = k;
    }
    if last != 63 {
        writer.put(u64::from(ac.code[0]), ac.length[0]);
    }
}

/// Returns the size category of `value` and its value bits (ITU T.81,
/// F.1.2.1).
#[inline(always)]
fn value_bits(value: i32) -> (usize, u32) {
    let size = 32 - value.unsigned_abs().leading_zeros();
    let bits = (value + (value >> 31)) as u32 & ((1 << size) - 1);
    (size as usize, bits)
}

/// A bit writer without byte stuffing. It stores the accumulator after each
/// code.
struct Writer {
    buf: Vec<u8>,
    /// The bytes written so far.
    pos: usize,
    acc: u64,
    /// The bits in `acc` after `pos`, 0 to 7.
    bits: u32,
}

impl Writer {
    fn new(buf: Vec<u8>, pos: usize) -> Self {
        Self {
            buf,
            pos,
            acc: 0,
            bits: 0,
        }
    }

    /// Makes room for `bytes` more bytes.
    #[inline(always)]
    fn reserve(&mut self, bytes: usize) {
        if self.buf.len() < self.pos + bytes + 8 {
            self.buf.resize(2 * self.buf.len() + bytes + 8, 0);
        }
    }

    /// Writes the low `size` bits of `code`, at most 32.
    #[inline(always)]
    fn put(&mut self, code: u64, size: u32) {
        self.acc = self.acc << size | code;
        self.bits += size;
        let bytes = (self.acc << (64 - self.bits)).to_be_bytes();
        self.buf[self.pos..self.pos + 8].copy_from_slice(&bytes);
        self.pos += (self.bits / 8) as usize;
        self.bits %= 8;
    }

    fn bit_len(&self) -> usize {
        8 * self.pos + self.bits as usize
    }

    /// Returns the bytes and the number of bits.
    fn finish(mut self) -> (Vec<u8>, usize) {
        let bits = self.bit_len();
        self.buf.truncate(bits.div_ceil(8));
        (self.buf, bits)
    }
}

/// Inserts a zero byte after each 0xFF byte in `buf[start..]`.
fn stuff(buf: &mut Vec<u8>, start: usize) {
    let count = memchr::memchr_iter(0xFF, &buf[start..]).count();
    if count == 0 {
        return;
    }
    let mut read = buf.len();
    let mut write = read + count;
    buf.resize(write, 0);
    while let Some(i) = memchr::memrchr(0xFF, &buf[start..read]) {
        let ff = start + i;
        let n = read - ff - 1;
        buf.copy_within(ff + 1..read, write - n);
        write -= n + 2;
        buf[write] = 0xFF;
        buf[write + 1] = 0;
        read = ff;
    }
}

/// Returns a frame with all quantization values 1 and `blocks` in MCU order.
/// Each value must be in the range -1023 to 1023.
#[cfg(test)]
pub(crate) fn test_frame(
    width: u16,
    height: u16,
    sampling: &[(u8, u8)],
    blocks: &[[i16; 64]],
) -> Vec<u8> {
    let encoder = Encoder::new(100);
    let components: Vec<Component> = (0..)
        .zip(sampling)
        .map(|(i, &(h, v))| Component {
            id: i + 1,
            h,
            v,
            quant: table(usize::from(i)) as u8,
            dc: table(usize::from(i)) as u8,
            ac: table(usize::from(i)) as u8,
        })
        .collect();
    let mut layout = Vec::new();
    for (i, c) in components.iter().enumerate() {
        let count = if components.len() == 1 {
            1
        } else {
            usize::from(c.h * c.v)
        };
        layout.extend(std::iter::repeat_n(i, count));
    }
    let mut writer = Writer::new(Vec::new(), 0);
    let mut predictors = [0; 4];
    for (g, block) in blocks.iter().enumerate() {
        let component = layout[g % layout.len()];
        let t = table(component);
        writer.reserve(BLOCK_BYTES);
        let nonzero = requant::nonzero(block);
        encode_block(
            &mut writer,
            block,
            nonzero,
            predictors[component],
            &encoder.dc[t],
            &encoder.ac[t],
        );
        predictors[component] = i32::from(block[0]);
    }
    let (bytes, bits) = writer.finish();
    let prepared = Prepared {
        encoder: &encoder,
        width,
        height,
        components,
    };
    let mut out = Vec::new();
    prepared.write(&[Encoded { bytes, bits }], &mut out);
    out
}
