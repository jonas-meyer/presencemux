//! The marker parser of a baseline JPEG frame.

use std::fmt;

use crate::decode::{Slot, Tables};
use crate::huffman::{Class, Spec};
use crate::{Error, Feature, MAX_BLOCKS};

/// One component of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Component {
    pub(crate) id: u8,
    pub(crate) h: u8,
    pub(crate) v: u8,
    /// The quantization table.
    pub(crate) quant: u8,
    /// The DC Huffman table of the scan.
    pub(crate) dc: u8,
    /// The AC Huffman table of the scan.
    pub(crate) ac: u8,
}

/// A parsed baseline JPEG frame with one interleaved scan.
pub struct Frame<'a> {
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// The components in scan order.
    pub(crate) components: Vec<Component>,
    /// The quantization tables, in zigzag order.
    pub(crate) quant: [Option<[u16; 64]>; 4],
    /// The decode tables of each block in an MCU.
    pub(crate) slots: Vec<Slot<'a>>,
    pub(crate) mcus: usize,
    /// The entropy-coded data, up to the marker after it.
    pub(crate) entropy: &'a [u8],
}

impl fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("components", &self.components)
            .field("blocks", &self.blocks())
            .field("entropy_bytes", &self.entropy.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Frame<'a> {
    /// Parses the headers of `data` and finds the entropy-coded data. Updates
    /// `tables` to the Huffman tables of the frame.
    pub fn parse(data: &'a [u8], tables: &'a mut Tables) -> Result<Self, Error> {
        if !data.starts_with(&[0xFF, 0xD8]) {
            return Err(Error::NotJpeg);
        }
        let mut quant = [None; 4];
        let mut dc: [Option<Spec>; 4] = Default::default();
        let mut ac: [Option<Spec>; 4] = Default::default();
        let mut sof: Option<(u16, u16, Vec<Component>)> = None;
        let mut pos = 2;
        loop {
            if data.get(pos) != Some(&0xFF) {
                return Err(match data.get(pos) {
                    None => Error::Truncated,
                    Some(_) => Error::Header("expected a marker"),
                });
            }
            // Fill bytes of 0xFF may come before a marker.
            while data.get(pos + 1) == Some(&0xFF) {
                pos += 1;
            }
            let marker = *data.get(pos + 1).ok_or(Error::Truncated)?;
            let length = data
                .get(pos + 2..pos + 4)
                .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
                .ok_or(Error::Truncated)?;
            if length < 2 {
                return Err(Error::Header("segment length"));
            }
            let segment = data
                .get(pos + 4..pos + 2 + length)
                .ok_or(Error::Truncated)?;
            pos += 2 + length;
            match marker {
                0xDB => parse_dqt(segment, &mut quant)?,
                0xC4 => parse_dht(segment, &mut dc, &mut ac)?,
                0xC0 | 0xC1 if sof.is_some() => return Err(Error::Header("two frame headers")),
                0xC0 | 0xC1 => sof = Some(parse_sof(segment)?),
                0xC2 | 0xC6 | 0xCA | 0xCE => return Err(Error::Unsupported(Feature::Progressive)),
                0xC3 | 0xC5 | 0xC7 | 0xC9 | 0xCB | 0xCD | 0xCF => {
                    return Err(Error::Unsupported(Feature::OtherCoding));
                }
                0xDD => {
                    if segment != [0, 0] {
                        return Err(Error::Unsupported(Feature::RestartMarkers));
                    }
                }
                0xDA => {
                    let (width, height, frame) =
                        sof.ok_or(Error::Header("scan before the frame header"))?;
                    let components = parse_sos(segment, &frame, &quant, &dc, &ac)?;
                    let entropy = &data[pos..end_of_entropy(data, pos)];
                    tables.update(dc, ac);
                    return Self::new(width, height, components, &quant, tables, entropy);
                }
                0x01 | 0xD0..=0xD9 => {
                    return Err(Error::Header("unexpected marker before the scan"));
                }
                _ => {}
            }
        }
    }

    fn new(
        width: u16,
        height: u16,
        components: Vec<Component>,
        quant: &[Option<[u16; 64]>; 4],
        tables: &'a Tables,
        entropy: &'a [u8],
    ) -> Result<Self, Error> {
        let (width_px, height_px) = (usize::from(width), usize::from(height));
        let (mcus, layout) = if let [_] = components[..] {
            // A scan with one component has one block in each MCU.
            (width_px.div_ceil(8) * height_px.div_ceil(8), vec![0])
        } else {
            let h_max = components
                .iter()
                .map(|c| usize::from(c.h))
                .max()
                .unwrap_or(1);
            let v_max = components
                .iter()
                .map(|c| usize::from(c.v))
                .max()
                .unwrap_or(1);
            let mut layout = Vec::new();
            for (i, c) in components.iter().enumerate() {
                layout.extend(std::iter::repeat_n(i, usize::from(c.h * c.v)));
            }
            if layout.len() > 10 {
                return Err(Error::Header("more than 10 blocks in an MCU"));
            }
            (
                width_px.div_ceil(8 * h_max) * height_px.div_ceil(8 * v_max),
                layout,
            )
        };
        if mcus * layout.len() > MAX_BLOCKS {
            return Err(Error::Unsupported(Feature::TooLarge));
        }
        let slots = layout
            .into_iter()
            .map(|component| tables.slot(component, components[component]))
            .collect::<Option<_>>()
            .ok_or(Error::Header("scan uses an undefined Huffman table"))?;
        Ok(Self {
            width,
            height,
            components,
            quant: *quant,
            slots,
            mcus,
            entropy,
        })
    }

    /// Returns the number of blocks of 8x8 samples in the frame.
    pub fn blocks(&self) -> usize {
        self.mcus * self.slots.len()
    }

    /// Returns the width and height in pixels.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
}

/// Returns the position of the last end-of-image marker after `start`, or the
/// end of `data` if there is none.
fn end_of_entropy(data: &[u8], start: usize) -> usize {
    let mut end = data.len();
    while let Some(i) = memchr::memrchr(0xFF, &data[start..end]) {
        if data.get(start + i + 1) == Some(&0xD9) {
            return start + i;
        }
        end = start + i;
    }
    data.len()
}

fn parse_dqt(mut segment: &[u8], quant: &mut [Option<[u16; 64]>; 4]) -> Result<(), Error> {
    while let [pq_tq, rest @ ..] = segment {
        let id = usize::from(pq_tq & 15);
        let (table, rest) = match pq_tq >> 4 {
            0 if rest.len() >= 64 => (std::array::from_fn(|k| u16::from(rest[k])), &rest[64..]),
            1 if rest.len() >= 128 => (
                std::array::from_fn(|k| u16::from_be_bytes([rest[2 * k], rest[2 * k + 1]])),
                &rest[128..],
            ),
            0 | 1 => return Err(Error::Header("quantization table length")),
            _ => return Err(Error::Header("quantization table precision")),
        };
        if id > 3 || table.contains(&0) {
            return Err(Error::Header("quantization table"));
        }
        quant[id] = Some(table);
        segment = rest;
    }
    Ok(())
}

fn parse_dht(
    mut segment: &[u8],
    dc: &mut [Option<Spec>; 4],
    ac: &mut [Option<Spec>; 4],
) -> Result<(), Error> {
    while let [tc_th, rest @ ..] = segment {
        let (class, id) = (tc_th >> 4, usize::from(tc_th & 15));
        let counts: [u8; 16] = rest
            .get(..16)
            .and_then(|c| c.try_into().ok())
            .ok_or(Error::Header("Huffman table length"))?;
        let total = counts.iter().map(|&n| usize::from(n)).sum::<usize>();
        let values = rest
            .get(16..16 + total)
            .ok_or(Error::Header("Huffman table length"))?;
        let (class, tables) = match class {
            0 => (Class::Dc, &mut *dc),
            1 => (Class::Ac, &mut *ac),
            _ => return Err(Error::Header("Huffman table class")),
        };
        let table = tables
            .get_mut(id)
            .ok_or(Error::Header("Huffman table id"))?;
        *table = Some(Spec::new(class, counts, values).map_err(Error::Header)?);
        segment = &rest[16 + total..];
    }
    Ok(())
}

fn parse_sof(segment: &[u8]) -> Result<(u16, u16, Vec<Component>), Error> {
    let [precision, h1, h0, w1, w0, count, rest @ ..] = segment else {
        return Err(Error::Header("frame header length"));
    };
    if *precision != 8 {
        return Err(Error::Unsupported(Feature::Precision));
    }
    let (height, width) = (
        u16::from_be_bytes([*h1, *h0]),
        u16::from_be_bytes([*w1, *w0]),
    );
    if height == 0 {
        return Err(Error::Unsupported(Feature::HeightFromDnl));
    }
    if width == 0 {
        return Err(Error::Header("frame width 0"));
    }
    if !(1..=4).contains(count) || rest.len() != 3 * usize::from(*count) {
        return Err(Error::Header("frame components"));
    }
    let mut components = Vec::with_capacity(rest.len() / 3);
    for c in rest.as_chunks::<3>().0 {
        let (h, v) = (c[1] >> 4, c[1] & 15);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || c[2] > 3 {
            return Err(Error::Header("component sampling or table"));
        }
        if components.iter().any(|other: &Component| other.id == c[0]) {
            return Err(Error::Header("two components with the same id"));
        }
        components.push(Component {
            id: c[0],
            h,
            v,
            quant: c[2],
            dc: 0,
            ac: 0,
        });
    }
    Ok((width, height, components))
}

fn parse_sos(
    segment: &[u8],
    frame: &[Component],
    quant: &[Option<[u16; 64]>; 4],
    dc: &[Option<Spec>; 4],
    ac: &[Option<Spec>; 4],
) -> Result<Vec<Component>, Error> {
    let [count, rest @ ..] = segment else {
        return Err(Error::Header("scan header length"));
    };
    let count = usize::from(*count);
    let selectors = rest
        .get(..2 * count)
        .ok_or(Error::Header("scan header length"))?;
    let &[ss, se, ah_al] = &rest[2 * count..] else {
        return Err(Error::Header("scan header length"));
    };
    if count != frame.len() {
        return Err(Error::Unsupported(Feature::MultipleScans));
    }
    if (ss, se, ah_al) != (0, 63, 0) {
        return Err(Error::Header("spectral selection of a sequential scan"));
    }
    let mut components = Vec::with_capacity(count);
    for s in selectors.as_chunks::<2>().0 {
        let mut c = *frame
            .iter()
            .find(|c| c.id == s[0])
            .ok_or(Error::Header("scan component not in the frame"))?;
        if components.iter().any(|other: &Component| other.id == c.id) {
            return Err(Error::Header("scan component twice"));
        }
        (c.dc, c.ac) = (s[1] >> 4, s[1] & 15);
        let defined = |tables: &[Option<Spec>; 4], id: u8| {
            tables.get(usize::from(id)).is_some_and(Option::is_some)
        };
        if !defined(dc, c.dc) || !defined(ac, c.ac) {
            return Err(Error::Header("scan uses an undefined Huffman table"));
        }
        if quant[usize::from(c.quant)].is_none() {
            return Err(Error::Header(
                "component uses an undefined quantization table",
            ));
        }
        components.push(c);
    }
    Ok(components)
}
