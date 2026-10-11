//! Tests of the decoder, the requantization and the encoder, on generated
//! frames and on saved camera frames.

#![expect(
    clippy::cast_possible_truncation,
    reason = "the test values fit in 16 bits"
)]

use proptest::prelude::*;

use crate::encode::test_frame;
use crate::{
    Coefficients, Encoded, Encoder, Error, Feature, Frame, Part, Prepared, Tables, decode,
};

/// A frame size and the sampling of each component.
#[derive(Clone, Debug)]
struct Shape {
    width: u16,
    height: u16,
    sampling: Vec<(u8, u8)>,
}

impl Shape {
    fn layout(&self) -> Vec<usize> {
        if self.sampling.len() == 1 {
            return vec![0];
        }
        let mut layout = Vec::new();
        for (i, &(h, v)) in self.sampling.iter().enumerate() {
            layout.extend(std::iter::repeat_n(i, usize::from(h * v)));
        }
        layout
    }

    fn blocks(&self) -> usize {
        let (width, height) = (usize::from(self.width), usize::from(self.height));
        if self.sampling.len() == 1 {
            return width.div_ceil(8) * height.div_ceil(8);
        }
        let h = usize::from(self.sampling.iter().map(|s| s.0).max().unwrap_or(1));
        let v = usize::from(self.sampling.iter().map(|s| s.1).max().unwrap_or(1));
        width.div_ceil(8 * h) * height.div_ceil(8 * v) * self.layout().len()
    }

    fn frame(&self, blocks: &[[i16; 64]]) -> Vec<u8> {
        test_frame(self.width, self.height, &self.sampling, blocks)
    }
}

fn shape() -> impl Strategy<Value = Shape> {
    let sampling = prop_oneof![
        // UVC cameras often send 4:2:2.
        Just(vec![(2, 1), (1, 1), (1, 1)]),
        Just(vec![(2, 2), (1, 1), (1, 1)]),
        Just(vec![(1, 1), (1, 1), (1, 1)]),
        Just(vec![(1, 1)]),
    ];
    (1u16..=160, 1u16..=96, sampling).prop_map(|(width, height, sampling)| Shape {
        width,
        height,
        sampling,
    })
}

/// A block with nonzero coefficients at random places.
fn block() -> impl Strategy<Value = [i16; 64]> {
    prop::collection::vec((0usize..64, -1023i16..=1023), 0..64).prop_map(|entries| {
        let mut block = [0; 64];
        for (k, value) in entries {
            block[k] = value;
        }
        block
    })
}

fn frame() -> impl Strategy<Value = (Shape, Vec<[i16; 64]>)> {
    shape().prop_flat_map(|shape| {
        let blocks = prop::collection::vec(block(), shape.blocks());
        (Just(shape), blocks)
    })
}

/// A source quantization table. Values up to 15 and larger values take
/// different paths in the requantization.
fn source_table() -> impl Strategy<Value = [u16; 64]> {
    prop_oneof![
        prop::array::uniform32(1u16..=15),
        prop::array::uniform32(16u16..=255),
    ]
    .prop_map(|values| std::array::from_fn(|k| values[k % 32]))
}

/// Returns the two 8-bit quantization tables of a frame that `Encoder` wrote.
fn quant_tables(data: &[u8]) -> [[u16; 64]; 2] {
    let first = find(data, 0xDB);
    [first + 5, first + 69 + 5].map(|start| std::array::from_fn(|k| u16::from(data[start + k])))
}

/// Decodes `data` in `count` parts and returns the blocks.
fn decoded(data: &[u8], count: usize) -> Result<Vec<[i16; 64]>, Error> {
    let mut tables = Tables::new();
    let frame = Frame::parse(data, &mut tables)?;
    let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
    Ok(decode(&frame, &mut parts)?.blocks().collect())
}

/// Requantizes `data` in `count` parts to `quality`.
fn requantized(data: &[u8], count: usize, quality: u8) -> Result<Vec<u8>, Error> {
    requantized_with(data, count, &mut Tables::new(), &mut Encoder::new(quality))
}

/// Requantizes `data` in `count` parts with `tables` and `encoder`.
fn requantized_with(
    data: &[u8],
    count: usize,
    tables: &mut Tables,
    encoder: &mut Encoder,
) -> Result<Vec<u8>, Error> {
    let frame = Frame::parse(data, tables)?;
    let mut parts: Vec<Part> = (0..count).map(|_| Part::new()).collect();
    let coefficients = decode(&frame, &mut parts)?;
    let prepared = encoder.prepare(&frame);
    let mut encoded: Vec<Encoded> = (0..count).map(|_| Encoded::new()).collect();
    for (index, out) in encoded.iter_mut().enumerate() {
        prepared.encode(&coefficients, index, out);
    }
    let mut out = Vec::new();
    prepared.write(&encoded, &mut out);
    Ok(out)
}

/// Returns `c * source / target`, rounded half to even and limited to 1023.
/// The magnitude of `c` is limited to 2047 first.
fn reference(c: i16, source: u16, target: u16) -> i16 {
    let c = c.clamp(-2047, 2047);
    let q = (f64::from(c) * f64::from(source) / f64::from(target)).round_ties_even();
    q.clamp(-1023.0, 1023.0) as i16
}

/// Returns the position of the first `marker` in `data`.
fn find(data: &[u8], marker: u8) -> usize {
    data.windows(2)
        .position(|w| w == [0xFF, marker])
        .expect("the marker")
}

/// Returns the end of the scan header of `data`.
fn scan_header_end(data: &[u8]) -> usize {
    let scan = find(data, 0xDA);
    scan + 2 + usize::from(u16::from_be_bytes([data[scan + 2], data[scan + 3]]))
}

/// Returns `data` with the 8-bit quantization tables `luma` and `chroma` in
/// place of the tables of `test_frame`.
fn with_quant_tables(data: &[u8], luma: &[u16; 64], chroma: &[u16; 64]) -> Vec<u8> {
    let mut data = data.to_vec();
    let first = find(&data, 0xDB);
    let second = first + 69;
    for (start, table) in [(first + 5, luma), (second + 5, chroma)] {
        for (byte, &value) in data[start..start + 64].iter_mut().zip(table) {
            *byte = value as u8;
        }
    }
    data
}

/// Returns `data` with 16-bit quantization tables that hold the same values
/// as the 8-bit tables of `test_frame`.
fn with_16_bit_quant_tables(data: &[u8]) -> Vec<u8> {
    let first = find(data, 0xDB);
    let mut out = data[..first].to_vec();
    for id in 0..2 {
        let table = &data[first + 69 * id + 5..first + 69 * id + 69];
        out.extend_from_slice(&[0xFF, 0xDB, 0, 131, 0x10 | id as u8]);
        out.extend(table.iter().flat_map(|&q| u16::from(q).to_be_bytes()));
    }
    out.extend_from_slice(&data[first + 2 * 69..]);
    out
}

/// Returns `data` with the Huffman tables 0 and 1 swapped in the DHT and SOS
/// segments. The frame then has other tables with the same blocks.
fn with_swapped_huffman_tables(data: &[u8]) -> Vec<u8> {
    let mut data = data.to_vec();
    let mut pos = find(&data, 0xC4);
    while data[pos..pos + 2] == [0xFF, 0xC4] {
        data[pos + 4] ^= 0x01;
        pos += 2 + usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
    }
    let scan = find(&data, 0xDA);
    let count = usize::from(data[scan + 4]);
    for selector in 0..count {
        data[scan + 6 + 2 * selector] ^= 0x11;
    }
    data
}

/// A random generator for large test frames.
struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A compile-time check that the types that threads share are `Send` and
/// `Sync`.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Frame<'static>>();
    shared::<Part>();
    shared::<Coefficients<'static>>();
    shared::<Prepared<'static>>();
    shared::<Encoded>();
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn decodes_the_blocks_that_the_encoder_wrote((shape, blocks) in frame()) {
        prop_assert_eq!(decoded(&shape.frame(&blocks), 1), Ok(blocks));
    }

    #[test]
    fn parts_never_give_wrong_blocks((shape, blocks) in frame(), count in 2usize..=8) {
        // Parts of a few bytes can be too short to synchronize.
        let result = decoded(&shape.frame(&blocks), count);
        prop_assert!(result == Ok(blocks) || result == Err(Error::NoSync), "{result:?}");
    }

    #[test]
    fn requantized_frame_holds_the_requantized_blocks(
        (shape, blocks) in frame(),
        luma in source_table(),
        chroma in source_table(),
        quality in 1u8..=100,
    ) {
        let data = with_quant_tables(&shape.frame(&blocks), &luma, &chroma);

        let out = requantized(&data, 1, quality)?;

        let targets = quant_tables(&out);
        let layout = shape.layout();
        let expected: Vec<[i16; 64]> = blocks
            .iter()
            .enumerate()
            .map(|(g, block)| {
                let (source, target) = if layout[g % layout.len()] == 0 {
                    (&luma, &targets[0])
                } else {
                    (&chroma, &targets[1])
                };
                std::array::from_fn(|k| reference(block[k], source[k], target[k]))
            })
            .collect();
        prop_assert_eq!(decoded(&out, 1), Ok(expected));
    }

    #[test]
    fn output_does_not_depend_on_the_parts((shape, blocks) in frame(), count in 2usize..=6) {
        let data = shape.frame(&blocks);
        let parts = requantized(&data, count, 80);
        prop_assume!(parts != Err(Error::NoSync));
        prop_assert_eq!(parts, requantized(&data, 1, 80));
    }

    #[test]
    fn truncated_frames_give_errors((shape, blocks) in frame(), cut in any::<prop::sample::Index>(), count in 1usize..=4) {
        let data = shape.frame(&blocks);
        let scan_end = scan_header_end(&data);
        // The cut keeps the headers and removes at least one byte of the
        // entropy-coded data.
        let keep = scan_end + cut.index(data.len() - 2 - scan_end);
        prop_assert!(decoded(&data[..keep], count).is_err());
    }

    #[test]
    fn corrupt_frames_do_not_panic(
        (shape, blocks) in frame(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 1..8),
        count in 1usize..=4,
    ) {
        let mut data = shape.frame(&blocks);
        for (index, flip) in flips {
            let i = index.index(data.len());
            data[i] ^= flip;
        }
        let _ = requantized(&data, count, 80);
    }

    #[test]
    fn truncated_headers_give_errors((shape, blocks) in frame(), cut in any::<prop::sample::Index>()) {
        let data = shape.frame(&blocks);
        let keep = cut.index(scan_header_end(&data));
        let mut tables = Tables::new();
        prop_assert!(Frame::parse(&data[..keep], &mut tables).is_err());
    }
}

/// Returns 4:2:2 blocks for a frame of `shape`, dense with large values, like
/// camera frames at full quality.
fn dense_blocks(shape: &Shape) -> Vec<[i16; 64]> {
    let mut random = XorShift(0x9E37_79B9_7F4A_7C15);
    (0..shape.blocks())
        .map(|_| {
            std::array::from_fn(|k| {
                let range = 1023 >> (k / 8);
                (random.next_u64() % (2 * range + 1)) as i16 - range as i16
            })
        })
        .collect()
}

#[test]
fn parts_synchronize_on_a_full_hd_frame() {
    let shape = Shape {
        width: 1920,
        height: 1080,
        sampling: vec![(2, 1), (1, 1), (1, 1)],
    };
    let blocks = dense_blocks(&shape);
    let data = shape.frame(&blocks);
    for count in 1..=8 {
        assert_eq!(decoded(&data, count).as_ref(), Ok(&blocks), "{count} parts");
    }
}

/// Returns a 4:2:2 frame of 64x32 pixels with dense blocks.
fn medium_frame() -> (Vec<u8>, Vec<[i16; 64]>) {
    let shape = Shape {
        width: 64,
        height: 32,
        sampling: vec![(2, 1), (1, 1), (1, 1)],
    };
    let blocks = dense_blocks(&shape);
    (shape.frame(&blocks), blocks)
}

#[test]
fn tables_follow_the_huffman_tables_of_each_frame() {
    let (data, blocks) = medium_frame();
    let swapped = with_swapped_huffman_tables(&data);
    let mut tables = Tables::new();
    let mut decode_with = |data: &[u8]| {
        let frame = Frame::parse(data, &mut tables).unwrap();
        let mut parts = [Part::new()];
        decode(&frame, &mut parts)
            .unwrap()
            .blocks()
            .collect::<Vec<_>>()
    };

    let results = [
        decode_with(&data),
        decode_with(&swapped),
        decode_with(&data),
    ];

    assert_eq!(results, [blocks.clone(), blocks.clone(), blocks]);
}

#[test]
fn encoder_follows_the_quantization_tables_of_each_frame() {
    let (data, _) = medium_frame();
    let other = with_quant_tables(&data, &[2; 64], &[3; 64]);
    let mut encoder = Encoder::new(80);
    let mut tables = Tables::new();

    let first = requantized_with(&data, 1, &mut tables, &mut encoder);
    let second = requantized_with(&other, 1, &mut tables, &mut encoder);

    assert_eq!(first, requantized(&data, 1, 80));
    assert_eq!(second, requantized(&other, 1, 80));
}

#[test]
fn set_quality_gives_the_output_of_a_new_encoder() {
    let (data, _) = medium_frame();
    let mut encoder = Encoder::new(80);
    let mut tables = Tables::new();
    requantized_with(&data, 1, &mut tables, &mut encoder).unwrap();

    encoder.set_quality(50);
    let out = requantized_with(&data, 1, &mut tables, &mut encoder);

    assert_eq!(out, requantized(&data, 1, 50));
}

#[test]
fn accepts_16_bit_quantization_tables() {
    let (data, _) = medium_frame();

    let out = requantized(&with_16_bit_quant_tables(&data), 1, 80);

    assert_eq!(out, requantized(&data, 1, 80));
}

#[test]
fn rejects_an_invalid_huffman_code() {
    let data = small_frame();
    let start = scan_header_end(&data);
    // Nine 1 bits are not a code of the standard luminance DC table.
    let invalid = [&data[..start], &[0xFF, 0x00, 0xFF, 0x00], &[0xFF, 0xD9][..]].concat();

    assert_eq!(decoded(&invalid, 1), Err(Error::Corrupt));
}

#[test]
fn rejects_a_frame_cut_in_the_scan() {
    let (data, _) = medium_frame();
    let cut = [&data[..data.len() / 2], &[0xFF, 0xD9][..]].concat();

    assert_eq!(decoded(&cut, 1), Err(Error::Truncated));
}

/// Returns a small valid frame with junk data after the last block.
fn frame_with_junk() -> Vec<u8> {
    let data = small_frame();
    let end = data.len() - 2;
    [&data[..end], &[0x55; 4000], &data[end..]].concat()
}

#[test]
fn junk_after_the_last_block_does_not_change_the_blocks() {
    assert_eq!(decoded(&frame_with_junk(), 1), Ok(vec![[0; 64]; 4]));
}

#[test]
fn parts_with_junk_after_the_last_block_never_give_wrong_blocks() {
    let result = decoded(&frame_with_junk(), 2);

    assert!(
        result == Ok(vec![[0; 64]; 4]) || result == Err(Error::NoSync),
        "{result:?}"
    );
}

#[test]
#[should_panic(expected = "no parts")]
fn decode_without_parts_panics() {
    let data = small_frame();
    let mut tables = Tables::new();
    let frame = Frame::parse(&data, &mut tables).unwrap();

    let _ = decode(&frame, &mut []);
}

#[test]
fn rejects_data_without_a_start_of_image() {
    let mut tables = Tables::new();
    let error = Frame::parse(b"\x89PNG\r\n\x1a\n", &mut tables).unwrap_err();

    assert_eq!(error, Error::NotJpeg);
}

/// Returns a small valid frame.
fn small_frame() -> Vec<u8> {
    let shape = Shape {
        width: 16,
        height: 8,
        sampling: vec![(2, 1), (1, 1), (1, 1)],
    };
    shape.frame(&[[0; 64]; 4])
}

/// Returns `data` with `segment` after the start-of-image marker.
fn with_segment(data: &[u8], segment: &[u8]) -> Vec<u8> {
    [&data[..2], segment, &data[2..]].concat()
}

#[test]
fn rejects_progressive_frames() {
    let mut data = small_frame();
    let sof = find(&data, 0xC0);
    data[sof + 1] = 0xC2;
    let mut tables = Tables::new();

    let error = Frame::parse(&data, &mut tables).unwrap_err();

    assert_eq!(error, Error::Unsupported(Feature::Progressive));
}

#[test]
fn rejects_restart_markers() {
    let data = with_segment(&small_frame(), &[0xFF, 0xDD, 0, 4, 0, 1]);
    let mut tables = Tables::new();

    let error = Frame::parse(&data, &mut tables).unwrap_err();

    assert_eq!(error, Error::Unsupported(Feature::RestartMarkers));
}

#[test]
fn accepts_a_restart_interval_of_zero() {
    let data = with_segment(&small_frame(), &[0xFF, 0xDD, 0, 4, 0, 0]);

    assert_eq!(decoded(&data, 1), Ok(vec![[0; 64]; 4]));
}

#[test]
fn rejects_12_bit_samples() {
    let mut data = small_frame();
    let sof = find(&data, 0xC0);
    data[sof + 4] = 12;
    let mut tables = Tables::new();

    let error = Frame::parse(&data, &mut tables).unwrap_err();

    assert_eq!(error, Error::Unsupported(Feature::Precision));
}

/// The count, sum, sum of magnitudes and sum of squares of the coefficients
/// of `blocks`.
fn statistics(blocks: &[[i16; 64]]) -> [i64; 4] {
    let values = || blocks.iter().flatten().map(|&c| i64::from(c));
    [
        i64::try_from(blocks.len()).unwrap(),
        values().sum(),
        values().map(i64::abs).sum(),
        values().map(|c| c * c).sum(),
    ]
}

/// The fixture is a 128x64 noise frame at 4:2:2 from GStreamer `jpegenc
/// quality=95` (libjpeg-turbo 2.1.5). libjpeg-turbo reads its coefficients with
/// these statistics.
#[test]
fn decodes_a_libjpeg_frame_to_the_coefficients_of_libjpeg() {
    let data = include_bytes!("../fixtures/libjpeg-422.jpg");

    let blocks = decoded(data, 1).unwrap();

    assert_eq!(statistics(&blocks), [256, -1539, 129_169, 6_017_925]);
}

#[test]
fn quality_80_gives_the_tables_of_libjpeg() {
    let (data, _) = medium_frame();

    let out = requantized(&data, 1, 80).unwrap();

    // libjpeg-turbo writes these tables for `jpeg_set_quality(80)`, in zigzag
    // order.
    let luma = [
        6, 4, 5, 6, 5, 4, 6, 6, 5, 6, 7, 7, 6, 8, 10, 16, 10, 10, 9, 9, 10, 20, 14, 15, 12, 16, 23,
        20, 24, 24, 23, 20, 22, 22, 26, 29, 37, 31, 26, 27, 35, 28, 22, 22, 32, 44, 32, 35, 38, 39,
        41, 42, 41, 25, 31, 45, 48, 45, 40, 48, 37, 40, 41, 40,
    ];
    let mut chroma = [40; 64];
    chroma[..14].copy_from_slice(&[7, 7, 7, 10, 8, 10, 19, 10, 10, 19, 40, 26, 22, 26]);
    assert_eq!(quant_tables(&out), [luma, chroma]);
}

/// Checks the decoder against the sum of the absolute coefficients that
/// libjpeg-turbo gives for the 150 saved camera frames.
#[test]
#[ignore = "needs the saved camera frames in the directory PRESENCEMUX_JPEG_FRAMES"]
fn saved_frames_give_the_coefficients_of_libjpeg() {
    let dir = std::env::var("PRESENCEMUX_JPEG_FRAMES").expect("PRESENCEMUX_JPEG_FRAMES");
    let frames: Vec<Vec<u8>> = (0..150)
        .map(|i| std::fs::read(format!("{dir}/f{i:03}.jpg")).expect("frame"))
        .collect();
    for count in [1, 3] {
        let sum: i64 = frames
            .iter()
            .flat_map(|data| decoded(data, count).expect("decode"))
            .flatten()
            .map(|c| i64::from(c).abs())
            .sum();

        assert_eq!(sum, 4_345_445_401, "{count} parts");
    }
}
