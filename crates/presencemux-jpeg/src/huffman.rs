//! The Huffman tables from DHT segments, and the lookup tables of the decoder.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "the codes, values and table entries of JPEG have fixed bit widths"
)]
#![expect(
    clippy::inline_always,
    reason = "the inner loops of the decoder and the encoder must inline"
)]

/// The class of a Huffman table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Dc,
    Ac,
}

/// A Huffman table from a DHT segment. It holds the number of codes of each
/// length and the symbols in code order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) counts: [u8; 16],
    pub(crate) values: Vec<u8>,
}

impl Spec {
    /// Returns the checked table. The codes must fit in their lengths. The value
    /// sizes must be at most 11 bits for DC and 10 bits for AC.
    pub(crate) fn new(class: Class, counts: [u8; 16], values: &[u8]) -> Result<Self, &'static str> {
        let total: usize = counts.iter().map(|&n| usize::from(n)).sum();
        if total != values.len() || total > 256 {
            return Err("Huffman table size");
        }
        let mut code = 0u32;
        for (length, &n) in (1..=16).zip(&counts) {
            code += u32::from(n);
            if code > 1 << length {
                return Err("Huffman codes do not fit their lengths");
            }
            code <<= 1;
        }
        let fits = |&symbol: &u8| match class {
            Class::Dc => symbol <= 11,
            Class::Ac => symbol & 15 <= 10,
        };
        if !values.iter().all(fits) {
            return Err("Huffman symbol larger than 8-bit samples allow");
        }
        Ok(Self {
            counts,
            values: values.to_vec(),
        })
    }

    /// Returns every code as (code, length, symbol), in code order.
    pub(crate) fn codes(&self) -> Vec<(u32, u32, u8)> {
        let mut codes = Vec::with_capacity(self.values.len());
        let mut symbols = self.values.iter();
        let mut code = 0u32;
        for (length, &n) in (1..=16).zip(&self.counts) {
            for &symbol in symbols.by_ref().take(usize::from(n)) {
                codes.push((code, length, symbol));
                code += 1;
            }
            code <<= 1;
        }
        codes
    }
}

/// The canonical code tables, for codes longer than a lookup.
#[derive(Debug)]
struct Canonical {
    /// The largest code of each length, or -1.
    max_code: [i32; 17],
    /// The index in `values` of each code, minus the code.
    offset: [i32; 17],
    values: Vec<u8>,
}

impl Canonical {
    fn new(spec: &Spec) -> Self {
        let mut max_code = [-1; 17];
        let mut offset = [0; 17];
        let (mut code, mut index) = (0i32, 0i32);
        for (length, &n) in (1..=16).zip(&spec.counts) {
            let n = i32::from(n);
            if n > 0 {
                offset[length] = index - code;
                code += n;
                index += n;
                max_code[length] = code - 1;
            }
            code <<= 1;
        }
        Self {
            max_code,
            offset,
            values: spec.values.clone(),
        }
    }

    /// Decodes one symbol from the top `available` bits of `bits`, with codes
    /// of at least `from` bits. Returns the symbol and the code length.
    fn decode(&self, bits: u64, available: u32, from: u32) -> Option<(u8, u32)> {
        for length in from..=available.min(16) {
            let code = (bits >> (available - length)) as i32;
            if code <= self.max_code[length as usize] {
                let index = (code + self.offset[length as usize]) as usize;
                return Some((self.values[index], length));
            }
        }
        None
    }
}

/// The lookup width of the DC table, in bits.
pub(crate) const DC_BITS: u32 = 9;
/// The lookup width of the AC table, in bits.
pub(crate) const AC_BITS: u32 = 12;

/// The decode table of a DC Huffman table.
#[derive(Debug)]
pub(crate) struct DcTable {
    /// The code length in the high byte and the symbol in the low byte, or 0
    /// for a code longer than `DC_BITS`.
    lookup: Box<[u16; 1 << DC_BITS]>,
    canonical: Canonical,
}

impl DcTable {
    pub(crate) fn new(spec: &Spec) -> Self {
        let mut lookup = Box::new([0; 1 << DC_BITS]);
        for (code, length, symbol) in spec.codes() {
            if length <= DC_BITS {
                let shift = DC_BITS - length;
                let start = (code << shift) as usize;
                lookup[start..start + (1 << shift)].fill((length as u16) << 8 | u16::from(symbol));
            }
        }
        Self {
            lookup,
            canonical: Canonical::new(spec),
        }
    }

    /// Returns the lookup entry for the next `DC_BITS` bits.
    #[inline(always)]
    pub(crate) fn lookup(&self, peek: usize) -> u16 {
        self.lookup[peek & ((1 << DC_BITS) - 1)]
    }

    /// Decodes a code longer than `DC_BITS` from the top bits of `buf`.
    /// Returns the symbol and the code length.
    #[cold]
    #[inline(never)]
    pub(crate) fn decode_long(&self, buf: u64) -> Option<(u8, u32)> {
        self.canonical.decode(buf >> 48, 16, DC_BITS + 1)
    }
}

/// An entry of the AC lookup. It decodes up to two symbols with their values.
///
/// Bits 0 to 5 hold the bits of both symbols, bit 6 `OTHER`, bit 7 `END`,
/// bits 8 to 15 the run of the first symbol, bits 16 to 31 its value, bits 32
/// to 39 the distance from the first to the second coefficient, bits 40 to 55
/// the second value, bits 56 to 61 the bits of the first symbol, and bit 62
/// `DOUBLE`. An `OTHER` entry holds the code length in bits 0 to 5 (0 for a
/// code longer than the lookup) and the symbol in bits 8 to 15.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Entry(u64);

impl Entry {
    /// The entry does not hold a complete symbol.
    const OTHER: u64 = 1 << 6;
    /// The block ends after this entry.
    const END: u64 = 1 << 7;
    /// The entry holds two symbols.
    const DOUBLE: u64 = 1 << 62;

    /// Returns an entry for a code without its value. `code` holds the symbol
    /// and the code length, or `None` for a code longer than the lookup.
    fn other(code: Option<(u8, u32)>) -> Self {
        Self(code.map_or(Self::OTHER, |(symbol, length)| {
            Self::OTHER | u64::from(symbol) << 8 | u64::from(length)
        }))
    }

    /// Returns an entry with one complete symbol.
    fn single(first: &Symbol) -> Self {
        let value = u64::from(first.value as i16 as u16);
        Self(
            u64::from(first.bits)
                | u64::from(first.run) << 8
                | value << 16
                | value << 40
                | u64::from(first.bits) << 56
                | if first.end { Self::END } else { 0 },
        )
    }

    /// Returns an entry with two complete symbols.
    fn double(first: &Symbol, second: &Symbol) -> Self {
        let value = u64::from(first.value as i16 as u16);
        // A second EOB stores the first value again at the same place.
        let (distance, second_value) = if second.end {
            (0, value)
        } else {
            (second.run + 1, u64::from(second.value as i16 as u16))
        };
        Self(
            u64::from(first.bits + second.bits)
                | u64::from(first.run) << 8
                | value << 16
                | u64::from(distance) << 32
                | second_value << 40
                | u64::from(first.bits) << 56
                | Self::DOUBLE
                | if second.end { Self::END } else { 0 },
        )
    }

    /// Returns whether the entry holds one or two complete symbols.
    #[inline(always)]
    pub(crate) fn is_complete(self) -> bool {
        self.0 & Self::OTHER == 0
    }

    /// Returns the bits of all symbols with their values, or the code length
    /// of an incomplete entry.
    #[inline(always)]
    pub(crate) fn bits(self) -> u32 {
        (self.0 & 63) as u32
    }

    /// Returns whether the block ends after this entry.
    #[inline(always)]
    pub(crate) fn ends(self) -> bool {
        self.0 & Self::END != 0
    }

    /// Returns whether the entry holds two symbols.
    #[inline(always)]
    pub(crate) fn is_double(self) -> bool {
        self.0 & Self::DOUBLE != 0
    }

    /// Returns the number of zeros before the first value.
    #[inline(always)]
    pub(crate) fn run(self) -> usize {
        (self.0 >> 8 & 0xFF) as usize
    }

    /// Returns the symbol of an incomplete entry.
    #[inline(always)]
    pub(crate) fn symbol(self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// Returns the first value.
    #[inline(always)]
    pub(crate) fn first(self) -> i16 {
        (self.0 >> 16) as i16
    }

    /// Returns the distance from the first to the second value.
    #[inline(always)]
    pub(crate) fn distance(self) -> usize {
        (self.0 >> 32 & 0xFF) as usize
    }

    /// Returns the second value, or the first value again.
    #[inline(always)]
    pub(crate) fn second(self) -> i16 {
        (self.0 >> 40) as i16
    }

    /// Returns the bits of the first symbol with its value.
    #[inline(always)]
    pub(crate) fn first_bits(self) -> u32 {
        (self.0 >> 56 & 63) as u32
    }
}

/// The decode table of an AC Huffman table.
#[derive(Debug)]
pub(crate) struct AcTable {
    lookup: Box<[Entry; 1 << AC_BITS]>,
    canonical: Canonical,
}

/// One complete AC symbol in a lookup, with its value.
struct Symbol {
    run: u32,
    value: i32,
    bits: u32,
    end: bool,
}

impl AcTable {
    pub(crate) fn new(spec: &Spec) -> Self {
        let canonical = Canonical::new(spec);
        let lookup: Box<[Entry]> = (0..1 << AC_BITS)
            .map(|i| Self::entry(&canonical, i))
            .collect();
        Self {
            lookup: lookup
                .try_into()
                .expect("the lookup has 1 << AC_BITS entries"),
            canonical,
        }
    }

    /// Returns the entry for the `AC_BITS` bits `i`.
    fn entry(canonical: &Canonical, i: u32) -> Entry {
        let Some(first) = Self::symbol(canonical, u64::from(i), AC_BITS) else {
            return Entry::other(canonical.decode(u64::from(i), AC_BITS, 1));
        };
        if first.end || first.bits >= AC_BITS {
            return Entry::single(&first);
        }
        let rest = AC_BITS - first.bits;
        match Self::symbol(canonical, u64::from(i & ((1 << rest) - 1)), rest) {
            Some(second) => Entry::double(&first, &second),
            None => Entry::single(&first),
        }
    }

    /// Decodes one complete symbol with its value from the top `available`
    /// bits of `bits`. Returns `None` if it does not fit.
    fn symbol(canonical: &Canonical, bits: u64, available: u32) -> Option<Symbol> {
        let (rs, length) = canonical.decode(bits, available, 1)?;
        let (run, size) = (u32::from(rs >> 4), u32::from(rs & 15));
        match (run, size) {
            // EOB stores a zero at the next coefficient, which is still zero.
            (0, 0) => Some(Symbol {
                run: 0,
                value: 0,
                bits: length,
                end: true,
            }),
            // ZRL stores a zero 15 coefficients ahead. The next symbol starts
            // after that zero.
            (15, 0) => Some(Symbol {
                run: 15,
                value: 0,
                bits: length,
                end: false,
            }),
            (_, 0) => None,
            _ if length + size > available => None,
            _ => {
                let raw = ((bits >> (available - length - size)) & ((1 << size) - 1)) as u32;
                Some(Symbol {
                    run,
                    value: extend(raw, size),
                    bits: length + size,
                    end: false,
                })
            }
        }
    }

    /// Returns the lookup entry for the next `AC_BITS` bits.
    #[inline(always)]
    pub(crate) fn lookup(&self, peek: usize) -> Entry {
        self.lookup[peek & ((1 << AC_BITS) - 1)]
    }

    /// Decodes a code longer than `AC_BITS` from the top bits of `buf`.
    /// Returns the symbol and the code length.
    #[cold]
    #[inline(never)]
    pub(crate) fn decode_long(&self, buf: u64) -> Option<(u8, u32)> {
        self.canonical.decode(buf >> 48, 16, AC_BITS + 1)
    }
}

/// Returns the signed value of the `size` raw bits `raw` (ITU T.81, F.2.2.1).
#[inline(always)]
pub(crate) fn extend(raw: u32, size: u32) -> i32 {
    let raw = raw as i32;
    if raw < 1 << (size - 1) {
        raw - (1 << size) + 1
    } else {
        raw
    }
}
