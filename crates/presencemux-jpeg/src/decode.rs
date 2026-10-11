//! The entropy decoder, for a whole frame or for parts of it.
//!
//! A part starts at a byte offset in the entropy-coded data. A part after the
//! first guesses that a block starts there, at the first block of an MCU. The
//! Huffman codes synchronize after a few blocks. Each part records the bit
//! position, the block and the DC predictors at the start of each of its first
//! blocks. A part before the last decodes past the end of its chunk and
//! records its blocks there too.
//!
//! Where two parts record the same bit position and the same block in the
//! MCU, both decoders have the same state. The DC predictors of the two parts
//! then differ by a constant, which gives the DC offset.

#![expect(
    clippy::cast_possible_truncation,
    reason = "the codes, values and table entries of JPEG have fixed bit widths"
)]
#![expect(
    clippy::inline_always,
    reason = "the inner loops of the decoder and the encoder must inline"
)]

use std::fmt;
use std::ops::Range;

use crate::Error;
use crate::header::{Component, Frame};
use crate::huffman::{AC_BITS, AcTable, DC_BITS, DcTable, Spec, extend};

/// The number of blocks that a part after the first records from its start.
const FIRST_RECORDS: usize = 256;
/// The number of blocks that a part before the last decodes past the end of
/// its chunk.
const OVERLAP_RECORDS: usize = 128;
/// The number of bytes past the end of its chunk that a part reads.
const MARGIN: usize = 32 * 1024;
/// The number of zero bytes after the data of a part.
const PADDING: usize = 8;

/// The Huffman decode tables of a frame.
///
/// Use one `Tables` for all frames. `Frame::parse` builds a table again only
/// when the frame changes it.
#[derive(Default)]
pub struct Tables {
    dc_specs: [Option<Spec>; 4],
    ac_specs: [Option<Spec>; 4],
    dc: [Option<DcTable>; 4],
    ac: [Option<AcTable>; 4],
}

impl fmt::Debug for Tables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tables").finish_non_exhaustive()
    }
}

impl Tables {
    /// Creates an empty set of tables.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the tables that differ from `dc` and `ac`.
    pub(crate) fn update(&mut self, dc: [Option<Spec>; 4], ac: [Option<Spec>; 4]) {
        for (i, spec) in dc.into_iter().enumerate() {
            if self.dc_specs[i] != spec {
                self.dc[i] = spec.as_ref().map(DcTable::new);
                self.dc_specs[i] = spec;
            }
        }
        for (i, spec) in ac.into_iter().enumerate() {
            if self.ac_specs[i] != spec {
                self.ac[i] = spec.as_ref().map(AcTable::new);
                self.ac_specs[i] = spec;
            }
        }
    }

    /// Returns the tables of a block of `component`. `index` is the place of
    /// the component in the scan.
    pub(crate) fn slot(&self, index: usize, component: Component) -> Option<Slot<'_>> {
        Some(Slot {
            dc: self.dc.get(usize::from(component.dc))?.as_ref()?,
            ac: self.ac.get(usize::from(component.ac))?.as_ref()?,
            component: index,
        })
    }
}

/// The decode tables of one block in an MCU.
#[derive(Clone, Copy)]
pub(crate) struct Slot<'a> {
    dc: &'a DcTable,
    ac: &'a AcTable,
    /// The component, as an index in the scan.
    pub(crate) component: usize,
}

impl fmt::Debug for Slot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("component", &self.component)
            .finish_non_exhaustive()
    }
}

/// The state of the decoder at the start of a block.
#[derive(Clone, Copy, Debug)]
struct Record {
    /// The bit position in the data of the part, without byte stuffing.
    bits: usize,
    /// The block, counted from the start of the part.
    block: usize,
    predictors: [i32; 4],
}

/// The decoded blocks of one part of a frame, and the records to join it with
/// the other parts. Keep it across frames to reuse its memory.
#[derive(Default)]
pub struct Part {
    /// The data of the part without byte stuffing, with zero padding.
    clean: Vec<u8>,
    /// The DCT coefficients of each block, in zigzag order. Each DC value is
    /// relative to the start of the part.
    blocks: Vec<[i16; 64]>,
    /// The bits of the chunk of this part, without byte stuffing.
    chunk_bits: usize,
    /// The records of the first blocks of the chunk.
    first: Vec<Record>,
    /// The records of the blocks after the end of the chunk.
    overlap: Vec<Record>,
    /// The block that failed to decode.
    failed: Option<usize>,
    /// The last block, if it read past the end of the data.
    overrun: Option<usize>,
}

impl fmt::Debug for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Part")
            .field("blocks", &self.blocks.len())
            .field("chunk_bits", &self.chunk_bits)
            .field("failed", &self.failed)
            .field("overrun", &self.overrun)
            .finish_non_exhaustive()
    }
}

impl Part {
    /// Creates an empty part.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes part `index` of `count` parts of `frame`.
    ///
    /// The parts are independent. Separate threads can decode them. `join`
    /// then checks them and gives the coefficients of the frame.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not less than `count`.
    pub fn decode(&mut self, frame: &Frame<'_>, index: usize, count: usize) {
        assert!(index < count, "part {index} of {count} parts");
        let last = index + 1 == count;
        let chunk = chunk(frame.entropy, index, count);
        self.clean.clear();
        unstuff(&frame.entropy[chunk.clone()], &mut self.clean);
        self.chunk_bits = 8 * self.clean.len();
        if !last {
            let margin = chunk.end..(chunk.end + MARGIN).min(frame.entropy.len());
            unstuff(&frame.entropy[margin], &mut self.clean);
        }
        let data_bits = 8 * self.clean.len();
        self.clean.extend_from_slice(&[0; PADDING]);
        self.blocks.clear();
        self.first.clear();
        self.overlap.clear();
        self.failed = None;
        self.overrun = None;

        let total = frame.blocks();
        let main_bits = if last { data_bits } else { self.chunk_bits };
        let mut cursor = Cursor::new(&self.clean, 0);
        if index > 0 {
            // A wrong guess can give a code that does not exist. The part then
            // starts again one bit later.
            let limit = Limit {
                blocks: FIRST_RECORDS,
                bits: main_bits,
                records: FIRST_RECORDS,
            };
            while let Err(failed) = run::<true>(
                &mut cursor,
                &frame.slots,
                &mut self.blocks,
                &mut self.first,
                &limit,
            ) {
                let restart = self.first[failed].bits + 1;
                self.blocks.clear();
                self.first.clear();
                cursor = Cursor::new(&self.clean, restart);
            }
        }
        let capacity = total + FIRST_RECORDS + OVERLAP_RECORDS;
        let limit = Limit {
            blocks: if index == 0 { total } else { capacity },
            bits: main_bits,
            records: 0,
        };
        let mut result = run::<false>(
            &mut cursor,
            &frame.slots,
            &mut self.blocks,
            &mut Vec::new(),
            &limit,
        );
        // Part 0 stops at the last block of the frame. With junk data after
        // that block, part 0 can stop before the end of its chunk.
        if result.is_ok() && !last && cursor.reader.consumed() >= self.chunk_bits {
            let limit = Limit {
                blocks: capacity,
                bits: data_bits,
                records: OVERLAP_RECORDS,
            };
            result = run::<true>(
                &mut cursor,
                &frame.slots,
                &mut self.blocks,
                &mut self.overlap,
                &limit,
            );
        }
        self.failed = result.err();
        if self.failed.is_none() && cursor.reader.consumed() > data_bits {
            self.overrun = self.blocks.len().checked_sub(1);
        }
    }
}

/// The DCT coefficients of a frame, in the blocks of its parts.
#[derive(Debug)]
pub struct Coefficients<'p> {
    parts: &'p [Part],
    spans: Vec<Span>,
    /// The component of each block in an MCU.
    layout: Vec<usize>,
}

/// The range of blocks of a part that belong to the frame.
#[derive(Clone, Debug)]
struct Span {
    part: usize,
    blocks: Range<usize>,
    /// The first block in the frame.
    first: usize,
    /// The DC offset of each component.
    offset: [i32; 4],
}

/// The blocks of one part that belong to the frame.
pub(crate) struct SpanView<'p> {
    /// The blocks, with DC values relative to the start of the part.
    pub(crate) blocks: &'p [[i16; 64]],
    /// The first block in the frame.
    pub(crate) first: usize,
    /// The DC offset of each component.
    pub(crate) offset: [i32; 4],
}

impl<'p> Coefficients<'p> {
    /// Returns the blocks of part `index`.
    pub(crate) fn span(&self, index: usize) -> Option<SpanView<'p>> {
        let span = self.spans.get(index)?;
        Some(SpanView {
            blocks: &self.parts[span.part].blocks[span.blocks.clone()],
            first: span.first,
            offset: span.offset,
        })
    }

    /// Returns the component of each block in an MCU.
    pub(crate) fn layout(&self) -> &[usize] {
        &self.layout
    }

    /// Returns the absolute DC value of the last block of `component` before
    /// block `before` of the frame.
    pub(crate) fn dc_before(&self, before: usize, component: usize) -> Option<i16> {
        let global = (before.saturating_sub(self.layout.len())..before)
            .rev()
            .find(|&g| self.layout[g % self.layout.len()] == component)?;
        let span = self
            .spans
            .iter()
            .find(|s| (s.first..s.first + s.blocks.len()).contains(&global))?;
        let block = &self.parts[span.part].blocks[span.blocks.start + global - span.first];
        Some(block[0].wrapping_add(span.offset[component] as i16))
    }

    /// Returns the blocks of the frame in MCU order, in zigzag order, with
    /// absolute DC values.
    pub fn blocks(&self) -> impl Iterator<Item = [i16; 64]> {
        let mut index = 0;
        self.spans.iter().flat_map(move |span| {
            let first = index;
            index += span.blocks.len();
            self.parts[span.part].blocks[span.blocks.clone()]
                .iter()
                .enumerate()
                .map(move |(i, block)| {
                    let component = self.layout[(first + i) % self.layout.len()];
                    let mut block = *block;
                    block[0] = block[0].wrapping_add(span.offset[component] as i16);
                    block
                })
        })
    }
}

/// Decodes `frame` with each part in turn, on the calling thread, and joins
/// the parts. With one part, the decoder reads the whole frame in one run.
///
/// # Errors
///
/// Returns the errors of `join`.
///
/// # Panics
///
/// Panics if `parts` is empty.
pub fn decode<'p>(frame: &Frame<'_>, parts: &'p mut [Part]) -> Result<Coefficients<'p>, Error> {
    let count = parts.len();
    assert!(count > 0, "no parts");
    for (index, part) in parts.iter_mut().enumerate() {
        part.decode(frame, index, count);
    }
    join(frame, parts)
}

/// Joins the decoded parts of `frame`.
///
/// # Errors
///
/// - `Error::NoSync` if two parts did not synchronize. Decode the frame again
///   with one part.
/// - `Error::Corrupt` if a block of the frame does not decode.
/// - `Error::Truncated` if the data ends before the last block.
///
/// # Panics
///
/// Panics if `parts` is empty.
pub fn join<'p>(frame: &Frame<'_>, parts: &'p [Part]) -> Result<Coefficients<'p>, Error> {
    assert!(!parts.is_empty(), "no parts");
    let total = frame.blocks();
    let per_mcu = frame.slots.len();
    let layout: Vec<usize> = frame.slots.iter().map(|slot| slot.component).collect();
    let mut spans = Vec::with_capacity(parts.len());
    let (mut start, mut offset, mut covered) = (0, [0i32; 4], 0);
    for (index, part) in parts.iter().enumerate() {
        let (end, next) = match parts.get(index + 1) {
            Some(next) => {
                let (mine, theirs) = sync(part, next, per_mcu).ok_or(Error::NoSync)?;
                let next_offset = std::array::from_fn(|c| {
                    offset[c]
                        .wrapping_add(mine.predictors[c])
                        .wrapping_sub(theirs.predictors[c])
                });
                (mine.block, Some((theirs.block, next_offset)))
            }
            None => (start + total.saturating_sub(covered), None),
        };
        if end < start || covered + (end - start) > total {
            return Err(Error::NoSync);
        }
        if part.failed.is_some_and(|block| block < end) {
            return Err(if index == 0 {
                Error::Corrupt
            } else {
                Error::NoSync
            });
        }
        if end > part.blocks.len() {
            return Err(Error::Truncated);
        }
        if part
            .overrun
            .is_some_and(|block| (start..end).contains(&block))
        {
            return Err(Error::Truncated);
        }
        spans.push(Span {
            part: index,
            blocks: start..end,
            first: covered,
            offset,
        });
        covered += end - start;
        if let Some((next_start, next_offset)) = next {
            (start, offset) = (next_start, next_offset);
        }
    }
    Ok(Coefficients {
        parts,
        spans,
        layout,
    })
}

/// Returns the first record after the chunk of `part` that matches a record
/// of `next`, with the same bit position and the same block in the MCU.
fn sync<'p>(part: &'p Part, next: &'p Part, per_mcu: usize) -> Option<(&'p Record, &'p Record)> {
    let mut theirs = next.first.iter().peekable();
    for mine in &part.overlap {
        let Some(bits) = mine.bits.checked_sub(part.chunk_bits) else {
            continue;
        };
        while theirs.next_if(|r| r.bits < bits).is_some() {}
        let other = theirs.peek()?;
        if other.bits == bits && other.block % per_mcu == mine.block % per_mcu {
            return Some((mine, other));
        }
    }
    None
}

/// Returns the byte range of chunk `index` of `count` in `entropy`. No range
/// starts between a 0xFF byte and its stuffed zero byte.
fn chunk(entropy: &[u8], index: usize, count: usize) -> Range<usize> {
    let start = |i: usize| {
        let at = entropy.len() * i / count;
        if at > 0 && entropy[at - 1] == 0xFF {
            at + 1
        } else {
            at
        }
        .min(entropy.len())
    };
    start(index)..start(index + 1)
}

/// Appends `data` to `out` without the stuffed zero bytes, up to the first
/// marker.
fn unstuff(data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len() + PADDING);
    let mut pos = 0;
    while let Some(i) = memchr::memchr(0xFF, &data[pos..]) {
        out.extend_from_slice(&data[pos..=pos + i]);
        pos += i + 1;
        if data.get(pos) != Some(&0x00) {
            // The 0xFF starts a marker or ends `data`.
            out.pop();
            return;
        }
        pos += 1;
    }
    out.extend_from_slice(&data[pos..]);
}

/// The limits of one run of the decoder.
struct Limit {
    blocks: usize,
    /// The decoder stops at the first block start at or after this bit.
    bits: usize,
    records: usize,
}

/// The state of the decoder between blocks.
#[derive(Clone, Copy)]
struct Cursor<'a> {
    reader: Reader<'a>,
    predictors: [i32; 4],
    /// The block in the MCU.
    phase: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], bit: usize) -> Self {
        Self {
            reader: Reader::new(data, bit),
            predictors: [0; 4],
            phase: 0,
        }
    }
}

/// Decodes blocks into `blocks` until a limit. With `RECORD`, records the
/// state at the start of each block. Returns the index of the block that
/// failed as the error.
#[inline(always)]
fn run<const RECORD: bool>(
    state: &mut Cursor<'_>,
    slots: &[Slot<'_>],
    blocks: &mut Vec<[i16; 64]>,
    records: &mut Vec<Record>,
    limit: &Limit,
) -> Result<(), usize> {
    let mut cursor = *state;
    let mut result = Ok(());
    while blocks.len() < limit.blocks
        && cursor.reader.consumed() < limit.bits
        && (!RECORD || records.len() < limit.records)
    {
        let index = blocks.len();
        if RECORD {
            records.push(Record {
                bits: cursor.reader.consumed(),
                block: index,
                predictors: cursor.predictors,
            });
        }
        let slot = &slots[cursor.phase];
        blocks.push([0; 64]);
        let predictor = &mut cursor.predictors[slot.component & 3];
        if decode_block(&mut cursor.reader, slot, predictor, &mut blocks[index]).is_err() {
            result = Err(index);
            break;
        }
        cursor.phase += 1;
        if cursor.phase == slots.len() {
            cursor.phase = 0;
        }
    }
    *state = cursor;
    result
}

/// Decodes one block. The DC value is the new predictor.
#[inline(always)]
fn decode_block(
    reader: &mut Reader<'_>,
    slot: &Slot<'_>,
    predictor: &mut i32,
    block: &mut [i16; 64],
) -> Result<(), ()> {
    reader.refill_below(27);
    let found = slot.dc.lookup(reader.peek(DC_BITS));
    let size = if found == 0 {
        let (symbol, length) = slot.dc.decode_long(reader.buf).ok_or(())?;
        reader.consume(length);
        u32::from(symbol)
    } else {
        reader.consume(u32::from(found >> 8));
        u32::from(found & 0xFF)
    };
    if size > 0 {
        reader.refill_below(27);
        *predictor = predictor.wrapping_add(reader.extend(size));
    }
    block[0] = *predictor as i16;

    // Each step reads at most 26 bits, and a refill gives at least 56.
    let mut k = 1;
    loop {
        reader.refill();
        if ac_step(reader, slot.ac, block, &mut k)? || ac_step(reader, slot.ac, block, &mut k)? {
            break;
        }
    }
    if k > 64 { Err(()) } else { Ok(()) }
}

/// Decodes one or two AC symbols. Returns whether the block ends.
#[inline(always)]
fn ac_step(
    reader: &mut Reader<'_>,
    table: &AcTable,
    block: &mut [i16; 64],
    k: &mut usize,
) -> Result<bool, ()> {
    let entry = table.lookup(reader.peek(AC_BITS));
    if entry.is_complete() {
        let saved = *reader;
        reader.consume(entry.bits());
        let k1 = *k + entry.run();
        block[k1 & 63] = entry.first();
        let k2 = k1 + entry.distance();
        block[k2.min(63)] = entry.second();
        *k = k2 + 1;
        if !entry.ends() && *k < 64 {
            return Ok(false);
        }
        if k1 == 63 && entry.is_double() {
            // The second symbol belongs to the next block.
            block[63] = entry.first();
            *reader = saved;
            reader.consume(entry.first_bits());
            *k = 64;
        }
        return Ok(true);
    }
    let symbol = if entry.bits() == 0 {
        let (symbol, length) = table.decode_long(reader.buf).ok_or(())?;
        reader.consume(length);
        symbol
    } else {
        reader.consume(entry.bits());
        entry.symbol()
    };
    let (run, size) = (usize::from(symbol >> 4), u32::from(symbol & 15));
    if size == 0 {
        if run != 15 {
            return Ok(true);
        }
        *k += 16;
    } else {
        *k += run;
        block[*k & 63] = reader.extend(size) as i16;
        *k += 1;
    }
    Ok(*k >= 64)
}

/// A bit reader on data without byte stuffing. The next bit is bit 63 of
/// `buf`.
#[derive(Clone, Copy)]
struct Reader<'a> {
    data: &'a [u8],
    /// The bytes loaded into `buf`.
    pos: usize,
    buf: u64,
    /// The valid bits in `buf`.
    bits: u32,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], bit: usize) -> Self {
        let mut reader = Self {
            data,
            pos: bit / 8,
            buf: 0,
            bits: 0,
        };
        reader.refill();
        reader.consume((bit % 8) as u32);
        reader
    }

    /// Returns the bits read so far.
    #[inline(always)]
    fn consumed(&self) -> usize {
        8 * self.pos - self.bits as usize
    }

    /// Fills `buf` to at least 56 valid bits, with no branch (F. Giesen,
    /// "Reading bits in far too many ways", variant 4). Past the end of the
    /// data, it reads zero bits.
    #[inline(always)]
    fn refill(&mut self) {
        let word = self
            .data
            .get(self.pos..)
            .and_then(<[u8]>::first_chunk::<8>)
            .map_or(0, |bytes| u64::from_be_bytes(*bytes));
        self.buf |= word >> self.bits;
        self.pos += ((63 - self.bits) / 8) as usize;
        self.bits |= 0x38;
    }

    #[inline(always)]
    fn refill_below(&mut self, bits: u32) {
        if self.bits < bits {
            self.refill();
        }
    }

    #[inline(always)]
    fn peek(&self, n: u32) -> usize {
        (self.buf >> (64 - n)) as usize
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf <<= n;
        self.bits -= n;
    }

    /// Reads a value of `size` bits, 1 to 11.
    #[inline(always)]
    fn extend(&mut self, size: u32) -> i32 {
        let raw = self.peek(size) as u32;
        self.consume(size);
        extend(raw, size)
    }
}
