//! Quantization tables and the requantization of coefficient blocks.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "the codes, values and table entries of JPEG have fixed bit widths"
)]
#![expect(
    clippy::inline_always,
    reason = "the inner loops of the decoder and the encoder must inline"
)]

/// The natural (row-major) index of each zigzag position.
pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The luminance table of ITU T.81, Annex K.1, in natural order.
const LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// The chrominance table of ITU T.81, Annex K.1, in natural order.
const CHROMA: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// The largest magnitude of a coefficient after the requantization. With it,
/// the DC differences fit in 11 bits and the AC values fit in 10 bits.
pub(crate) const LIMIT: u16 = 1023;
/// The largest magnitude of a source coefficient.
const SOURCE_LIMIT: u16 = 2047;

/// Returns the luminance and chrominance tables of `quality` in zigzag order,
/// scaled like `jpeg_set_quality` of libjpeg with `force_baseline`.
pub(crate) fn tables(quality: u8) -> [[u16; 64]; 2] {
    let quality = u32::from(quality.clamp(1, 100));
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - 2 * quality
    };
    [LUMA, CHROMA].map(|base| {
        std::array::from_fn(|k| {
            let value = (u32::from(base[ZIGZAG[k]]) * scale + 50) / 100;
            value.clamp(1, 255) as u16
        })
    })
}

/// The requantization of one block from a source table to a target table, in
/// zigzag order. It rounds half to even.
///
/// For a coefficient `c`, let `n = 2 |c| qs + qt` and `d = 2 qt`. Then
/// `n / d`, rounded down, is `|c| qs / qt` rounded half up. A multiplication
/// by the reciprocal of `d` replaces the division.
#[derive(Clone, Debug)]
pub(crate) struct Requant {
    source: [u16; 64],
    /// Twice the source value, limited to `u16::MAX`.
    source2: [u16; 64],
    target: [u16; 64],
    /// The divisor `2 qt`.
    divisor: [u16; 64],
    /// The reciprocal of the divisor, scaled by `2^shift`.
    reciprocal: [u16; 64],
    shift: [u32; 64],
    /// Whether the multiplication gives the exact quotient for each source
    /// value.
    exact: bool,
}

impl Requant {
    pub(crate) fn new(source: &[u16; 64], target: &[u16; 64]) -> Self {
        let mut requant = Self {
            source: *source,
            source2: [0; 64],
            target: *target,
            divisor: [0; 64],
            reciprocal: [0; 64],
            shift: [0; 64],
            exact: true,
        };
        for k in 0..64 {
            let (qs, qt) = (u32::from(source[k]), u32::from(target[k]));
            let divisor = 2 * qt;
            // The shift is the largest with a reciprocal that fits in 16 bits.
            let mut shift = 16;
            while shift < 31 && (1u64 << (shift + 1)).div_ceil(u64::from(divisor)) < 1 << 16 {
                shift += 1;
            }
            let reciprocal = (1u32 << shift).div_ceil(divisor);
            let fits = 2 * u32::from(SOURCE_LIMIT) * qs + qt < 1 << 16;
            requant.exact &= fits
                && (0..=u32::from(SOURCE_LIMIT)).all(|magnitude| {
                    let numerator = 2 * magnitude * qs + qt;
                    (numerator * reciprocal) >> shift == numerator / divisor
                });
            requant.source2[k] = (2 * qs).min(u32::from(u16::MAX)) as u16;
            requant.divisor[k] = divisor as u16;
            requant.reciprocal[k] = reciprocal as u16;
            requant.shift[k] = shift;
        }
        requant
    }

    /// Requantizes `block` into `out`, with `dc_offset` added to the DC value
    /// first. Returns a mask with bit 63 - k set when coefficient k of `out` is
    /// not zero.
    #[inline(always)]
    pub(crate) fn apply(&self, block: &[i16; 64], dc_offset: i16, out: &mut [i16; 64]) -> u64 {
        let dc = block[0].wrapping_add(dc_offset);
        if self.exact {
            for k in 0..64 {
                out[k] = self.multiply(k, block[k]);
            }
            out[0] = self.multiply(0, dc);
        } else {
            for k in 0..64 {
                out[k] = self.one(k, block[k]);
            }
            out[0] = self.one(0, dc);
        }
        nonzero(out)
    }

    /// Requantizes the coefficient `c` at zigzag position `k`, with the
    /// reciprocal. Valid only when `exact` is true.
    #[inline(always)]
    fn multiply(&self, k: usize, c: i16) -> i16 {
        let magnitude = c.unsigned_abs().min(SOURCE_LIMIT);
        let numerator = magnitude * self.source2[k] + self.target[k];
        let product = u32::from(numerator) * u32::from(self.reciprocal[k]);
        let mut quotient = (product >> self.shift[k]) as u16;
        let tie = u16::from(quotient * self.divisor[k] == numerator);
        quotient -= tie & quotient & 1;
        let quotient = quotient.min(LIMIT) as i16;
        if c < 0 { -quotient } else { quotient }
    }

    /// Requantizes the coefficient `c` at zigzag position `k`, with a
    /// division.
    pub(crate) fn one(&self, k: usize, c: i16) -> i16 {
        let magnitude = u32::from(c.unsigned_abs().min(SOURCE_LIMIT));
        let target = u32::from(self.target[k]);
        let numerator = 2 * magnitude * u32::from(self.source[k]) + target;
        let divisor = 2 * target;
        let mut quotient = numerator / divisor;
        if quotient * divisor == numerator {
            quotient &= !1;
        }
        let quotient = quotient.min(u32::from(LIMIT)) as i16;
        if c < 0 { -quotient } else { quotient }
    }
}

/// Returns a mask with bit 63 - k set when coefficient k is not zero.
#[inline(always)]
pub(crate) fn nonzero(block: &[i16; 64]) -> u64 {
    let flags: [u8; 64] = std::array::from_fn(|k| u8::from(block[k] != 0));
    let words: [u64; 8] = zerocopy::transmute!(flags);
    // The multiplication moves bit 0 of byte j of a word to bit 63 - j.
    words.iter().fold(0, |mask, &word| {
        mask << 8 | u64::from_le(word).wrapping_mul(0x8040_2010_0804_0201) >> 56
    })
}
