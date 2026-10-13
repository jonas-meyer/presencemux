//! The images of the keys, the touch points and the info bar. It turns views
//! into JPEG images, rotated by 180° for the device.

use std::io;
use std::path::{Path, PathBuf};

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont as _, point};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::view::{InfoBarView, KeyView, PlaneTone, Tone};

/// The directory of the fonts, from the package `fonts-dejavu-core`.
pub(crate) const FONT_DIR: &str = "/usr/share/fonts/truetype/dejavu";

/// The JPEG quality of the images.
const QUALITY: u8 = 95;

/// The size of a key image in pixels.
const KEY_SIZE: u32 = 96;

/// The size of the info bar image in pixels.
const INFO_BAR_SIZE: (u32, u32) = (248, 58);

/// The text width of a key in pixels, with a margin of 5 pixels on each side.
const KEY_TEXT_WIDTH: f32 = 86.0;

/// The smallest text size in pixels. Longer text overflows the image.
const MIN_TEXT_SIZE: f32 = 10.0;

const WHITE: Rgb = Rgb(0xff, 0xff, 0xff);
const LIGHT_GREY: Rgb = Rgb(0xc8, 0xc8, 0xc8);
const GREY: Rgb = Rgb(0x80, 0x80, 0x80);
const DIM: Rgb = Rgb(0x5a, 0x5a, 0x5a);
const INK: Rgb = Rgb(0x14, 0x14, 0x14);
const STRIPE: Rgb = Rgb(0x6a, 0x6a, 0x6a);
const BLACK: Rgb = Rgb(0, 0, 0);
/// The background of a lit key that sets only privacy.
const LIT: Rgb = Rgb(0xe8, 0xe8, 0xe8);

/// A color without transparency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb(u8, u8, u8);

impl Rgb {
    pub(crate) const fn bytes(self) -> [u8; 3] {
        [self.0, self.1, self.2]
    }

    fn paint(self) -> Paint<'static> {
        let mut paint = Paint::default();
        paint.set_color_rgba8(self.0, self.1, self.2, 0xff);
        paint
    }
}

/// Returns the background color of a plane key in `tone`.
const fn plane_color(tone: PlaneTone) -> Rgb {
    match tone {
        PlaneTone::Live => Rgb(0xe0, 0x1e, 0x1e),
        PlaneTone::Canned => Rgb(0xf2, 0xa9, 0x00),
        PlaneTone::Off => Rgb(0x1c, 0x1c, 0x1e),
        PlaneTone::Unknown => Rgb(0x50, 0x50, 0x50),
    }
}

/// Returns the background color of `tone`.
pub(crate) const fn background(tone: Tone) -> Rgb {
    match tone {
        Tone::Plane(tone) => plane_color(tone),
        Tone::Private => LIT,
        Tone::Neutral => Rgb(0x22, 0x2a, 0x36),
        Tone::Dark => BLACK,
    }
}

/// Returns the text color on the background of `tone`.
const fn ink(tone: Tone) -> Rgb {
    match tone {
        Tone::Plane(PlaneTone::Live | PlaneTone::Unknown) | Tone::Neutral => WHITE,
        Tone::Plane(PlaneTone::Canned) | Tone::Private => INK,
        Tone::Plane(PlaneTone::Off) => LIGHT_GREY,
        Tone::Dark => DIM,
    }
}

/// Returns the color of the dot of a plane in `tone`, in the info bar.
const fn dot(tone: PlaneTone) -> Rgb {
    match tone {
        PlaneTone::Live | PlaneTone::Canned => plane_color(tone),
        PlaneTone::Unknown => GREY,
        PlaneTone::Off => DIM,
    }
}

/// Returns the color of a touch point that shows `tone`. Off is black.
pub(crate) const fn touch_point(tone: PlaneTone) -> Rgb {
    match tone {
        PlaneTone::Live | PlaneTone::Canned => plane_color(tone),
        PlaneTone::Unknown => Rgb(0x40, 0x40, 0x40),
        PlaneTone::Off => BLACK,
    }
}

/// A font that cannot be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FontError {
    #[error("cannot read the font {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("the font {} is not a valid font", path.display())]
    Invalid { path: PathBuf },
}

/// The fonts of the images.
pub(crate) struct Fonts {
    bold: FontVec,
    regular: FontVec,
}

impl Fonts {
    /// Reads DejaVu Sans Bold and DejaVu Sans from `dir`.
    pub(crate) fn load(dir: &Path) -> Result<Self, FontError> {
        Ok(Self {
            bold: font(&dir.join("DejaVuSans-Bold.ttf"))?,
            regular: font(&dir.join("DejaVuSans.ttf"))?,
        })
    }
}

/// Reads the font at `path`.
fn font(path: &Path) -> Result<FontVec, FontError> {
    let bytes = std::fs::read(path).map_err(|source| FontError::Read {
        path: path.to_owned(),
        source,
    })?;
    FontVec::try_from_vec(bytes).map_err(|_| FontError::Invalid {
        path: path.to_owned(),
    })
}

/// An image that the JPEG encoder cannot encode.
#[derive(Debug, thiserror::Error)]
#[error("cannot encode the image: {0}")]
pub(crate) struct EncodeError(#[from] jpeg_encoder::EncodingError);

/// Returns the JPEG image of a key with `view`.
pub(crate) fn key(fonts: &Fonts, view: &KeyView) -> Result<Vec<u8>, EncodeError> {
    jpeg(&key_pixmap(fonts, view))
}

/// Returns the JPEG image of the info bar with `view`.
pub(crate) fn info_bar(fonts: &Fonts, view: &InfoBarView) -> Result<Vec<u8>, EncodeError> {
    jpeg(&info_bar_pixmap(fonts, view))
}

/// Returns the image of a key with `view`, before the rotation.
fn key_pixmap(fonts: &Fonts, view: &KeyView) -> Pixmap {
    let mut pixmap = pixmap(KEY_SIZE, KEY_SIZE);
    let fill = background(view.tone);
    let text = ink(view.tone);
    pixmap.fill(color(fill));
    if view.tone == Tone::Plane(PlaneTone::Unknown) {
        stripes(&mut pixmap);
        // A plain band keeps the title readable over the stripes.
        fill_rect(&mut pixmap, Rect::from_xywh(0.0, 34.0, 96.0, 30.0), fill);
    }
    if let Some(label) = view.label {
        let label_ink = if view.tone == Tone::Plane(PlaneTone::Off) {
            GREY
        } else {
            text
        };
        draw_text(&mut pixmap, &fonts.bold, 13.0, label, 48.0, 22.0, label_ink);
    }
    let size = fit(&fonts.bold, 24.0, KEY_TEXT_WIDTH, &view.title);
    draw_text(
        &mut pixmap,
        &fonts.bold,
        size,
        &view.title,
        48.0,
        58.0,
        text,
    );
    if let Some(detail) = &view.detail {
        let size = fit(&fonts.bold, 13.0, KEY_TEXT_WIDTH, detail);
        draw_text(&mut pixmap, &fonts.bold, size, detail, 48.0, 82.0, text);
    }
    pixmap
}

/// Returns the image of the info bar with `view`, before the rotation.
fn info_bar_pixmap(fonts: &Fonts, view: &InfoBarView) -> Pixmap {
    let (width, height) = INFO_BAR_SIZE;
    let mut pixmap = pixmap(width, height);
    pixmap.fill(color(BLACK));
    for (segment, center) in [(&view.cam, 62.0), (&view.mic, 186.0)] {
        let size = fit(&fonts.bold, 16.0, 100.0, &segment.text);
        let width = text_width(&fonts.bold, size, &segment.text);
        // The dot and a gap go before the text. The group is centered on `center`.
        let left = center - f32::midpoint(width, 16.0);
        fill_circle(&mut pixmap, left + 5.0, 18.0, 5.0, dot(segment.tone));
        let text_center = left + 16.0 + width / 2.0;
        draw_text(
            &mut pixmap,
            &fonts.bold,
            size,
            &segment.text,
            text_center,
            24.0,
            WHITE,
        );
    }
    let size = fit(&fonts.regular, 14.0, 236.0, &view.bottom);
    draw_text(
        &mut pixmap,
        &fonts.regular,
        size,
        &view.bottom,
        124.0,
        48.0,
        LIGHT_GREY,
    );
    pixmap
}

/// Returns an empty image of `width` by `height` pixels.
fn pixmap(width: u32, height: u32) -> Pixmap {
    Pixmap::new(width, height).expect("the image sizes are constants above 0")
}

fn color(rgb: Rgb) -> Color {
    Color::from_rgba8(rgb.0, rgb.1, rgb.2, 0xff)
}

/// Fills `rect` with `rgb`. A rectangle without area draws nothing.
fn fill_rect(pixmap: &mut Pixmap, rect: Option<Rect>, rgb: Rgb) {
    if let Some(rect) = rect {
        pixmap.fill_rect(rect, &rgb.paint(), Transform::identity(), None);
    }
}

/// Fills a circle around `x`, `y` with the radius `radius`.
fn fill_circle(pixmap: &mut Pixmap, x: f32, y: f32, radius: f32, rgb: Rgb) {
    if let Some(circle) = PathBuilder::from_circle(x, y, radius) {
        let mut paint = rgb.paint();
        paint.anti_alias = true;
        pixmap.fill_path(
            &circle,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Draws diagonal stripes over the whole image.
fn stripes(pixmap: &mut Pixmap) {
    let mut path = PathBuilder::new();
    for x in (-96_i16..192).step_by(16) {
        path.move_to(f32::from(x), 96.0);
        path.line_to(f32::from(x) + 96.0, 0.0);
    }
    if let Some(path) = path.finish() {
        let stroke = Stroke {
            width: 5.0,
            ..Stroke::default()
        };
        let mut paint = STRIPE.paint();
        paint.anti_alias = true;
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Returns the width of `text` in `font` at the size `size` in pixels.
fn text_width(font: &FontVec, size: f32, text: &str) -> f32 {
    let font = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for c in text.chars() {
        let glyph = font.glyph_id(c);
        if let Some(previous) = previous {
            width += font.kern(previous, glyph);
        }
        width += font.h_advance(glyph);
        previous = Some(glyph);
    }
    width
}

/// Returns the largest size from `max` down to [`MIN_TEXT_SIZE`] at which
/// `text` fits in `width`.
fn fit(font: &FontVec, max: f32, width: f32, text: &str) -> f32 {
    let mut size = max;
    while size > MIN_TEXT_SIZE && text_width(font, size, text) > width {
        size -= 1.0;
    }
    size
}

/// Draws one line of `text` centered on `center`, with its baseline at
/// `baseline`. The image must be opaque.
fn draw_text(
    pixmap: &mut Pixmap,
    font: &FontVec,
    size: f32,
    text: &str,
    center: f32,
    baseline: f32,
    rgb: Rgb,
) {
    let width = pixmap.width();
    let height = pixmap.height();
    let data = pixmap.data_mut();
    let scaled = font.as_scaled(PxScale::from(size));
    // A start on a whole pixel keeps the glyph edges sharp.
    let mut x = (center - text_width(font, size, text) / 2.0).round();
    let mut previous = None;
    for c in text.chars() {
        let glyph = scaled.glyph_id(c);
        if let Some(previous) = previous {
            x += scaled.kern(previous, glyph);
        }
        let positioned = glyph.with_scale_and_position(size, point(x, baseline));
        x += scaled.h_advance(glyph);
        previous = Some(glyph);
        let Some(outline) = font.outline_glyph(positioned) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|glyph_x, glyph_y, coverage| {
            let Some(x) = pixel(bounds.min.x, glyph_x, width) else {
                return;
            };
            let Some(y) = pixel(bounds.min.y, glyph_y, height) else {
                return;
            };
            let index = (y * width as usize + x) * 4;
            // The premultiplied bytes of an opaque image are plain RGB.
            for (byte, value) in data[index..index + 3].iter_mut().zip(rgb.bytes()) {
                *byte = blend(*byte, value, coverage);
            }
        });
    }
}

/// Returns the pixel at `offset` from the glyph edge `edge`, or `None` outside
/// an image of `size` pixels.
#[expect(
    clippy::cast_possible_truncation,
    reason = "glyph edges are small whole pixel values"
)]
fn pixel(edge: f32, offset: u32, size: u32) -> Option<usize> {
    let pixel = i64::from(edge as i32) + i64::from(offset);
    usize::try_from(pixel)
        .ok()
        .filter(|&pixel| pixel < size as usize)
}

/// Returns `under` covered by `over` to the fraction `coverage`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the result is rounded and clamped to 0..=255"
)]
fn blend(under: u8, over: u8, coverage: f32) -> u8 {
    let coverage = coverage.clamp(0.0, 1.0);
    let value = f32::from(under) * (1.0 - coverage) + f32::from(over) * coverage;
    value.round().clamp(0.0, 255.0) as u8
}

/// Returns `pixmap` as a JPEG image, rotated by 180°.
fn jpeg(pixmap: &Pixmap) -> Result<Vec<u8>, EncodeError> {
    let rgb = rotated_rgb(pixmap);
    let mut jpeg = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, QUALITY);
    // Full color resolution keeps the edges of red and amber sharp.
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    let width = u16::try_from(pixmap.width()).unwrap_or(u16::MAX);
    let height = u16::try_from(pixmap.height()).unwrap_or(u16::MAX);
    encoder.encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb)?;
    Ok(jpeg)
}

/// Returns the RGB bytes of the opaque `pixmap`, rotated by 180°.
fn rotated_rgb(pixmap: &Pixmap) -> Vec<u8> {
    pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .rev()
        .flat_map(|&[red, green, blue, _]| [red, green, blue])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::load(Path::new(FONT_DIR)).unwrap()
    }

    /// Returns the color of the pixel at `x`, `y` of `pixmap`.
    fn rgb_at(pixmap: &Pixmap, x: u32, y: u32) -> Rgb {
        let pixel = pixmap.pixel(x, y).unwrap();
        Rgb(pixel.red(), pixel.green(), pixel.blue())
    }

    fn view(tone: Tone, title: &str) -> KeyView {
        KeyView {
            tone,
            label: None,
            title: title.to_owned(),
            detail: None,
        }
    }

    #[test]
    fn live_key_has_a_red_background() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Plane(PlaneTone::Live), "ON AIR"));

        assert_eq!(rgb_at(&pixmap, 1, 1), Rgb(0xe0, 0x1e, 0x1e));
    }

    #[test]
    fn private_key_is_lit() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Private, "PRIVACY"));

        assert_eq!(rgb_at(&pixmap, 1, 1), Rgb(0xe8, 0xe8, 0xe8));
    }

    #[test]
    fn slate_key_has_an_amber_background() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Plane(PlaneTone::Canned), "BRB"));

        assert_eq!(rgb_at(&pixmap, 1, 1), Rgb(0xf2, 0xa9, 0x00));
    }

    #[test]
    fn live_touch_point_is_red() {
        assert_eq!(touch_point(PlaneTone::Live), Rgb(0xe0, 0x1e, 0x1e));
    }

    #[test]
    fn slate_touch_point_is_amber() {
        assert_eq!(touch_point(PlaneTone::Canned), Rgb(0xf2, 0xa9, 0x00));
    }

    #[test]
    fn off_touch_point_is_black() {
        assert_eq!(touch_point(PlaneTone::Off), Rgb(0, 0, 0));
    }

    #[test]
    fn unknown_touch_point_is_dark_grey() {
        assert_eq!(touch_point(PlaneTone::Unknown), Rgb(0x40, 0x40, 0x40));
    }

    #[test]
    fn slate_key_has_dark_text() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Plane(PlaneTone::Canned), "BRB"));

        let row: Vec<Rgb> = (0..96).map(|x| rgb_at(&pixmap, x, 50)).collect();
        assert!(row.contains(&Rgb(0x14, 0x14, 0x14)));
    }

    #[test]
    fn unknown_key_keeps_a_plain_band_behind_the_title() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Plane(PlaneTone::Unknown), ""));

        assert_eq!(rgb_at(&pixmap, 4, 40), Rgb(0x50, 0x50, 0x50));
    }

    #[test]
    fn unknown_key_has_stripes_outside_the_band() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Plane(PlaneTone::Unknown), ""));

        let row: Vec<Rgb> = (0..96).map(|x| rgb_at(&pixmap, x, 10)).collect();
        assert!(row.contains(&Rgb(0x6a, 0x6a, 0x6a)));
    }

    #[test]
    fn title_draws_white_pixels_in_the_middle_of_the_key() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Neutral, "LIVE"));

        let row: Vec<Rgb> = (0..96).map(|x| rgb_at(&pixmap, x, 50)).collect();
        assert!(row.contains(&Rgb(0xff, 0xff, 0xff)));
    }

    #[test]
    fn long_title_leaves_the_edges_of_the_key_free() {
        let pixmap = key_pixmap(&fonts(), &view(Tone::Neutral, "STILL-HERE"));

        let edges: Vec<Rgb> = (36..64)
            .flat_map(|y| [0, 1, 94, 95].map(|x| rgb_at(&pixmap, x, y)))
            .collect();
        assert_eq!(edges, vec![Rgb(0x22, 0x2a, 0x36); 28 * 4]);
    }

    #[test]
    fn rotation_reverses_the_pixels() {
        let mut pixmap = pixmap(2, 1);
        fill_rect(
            &mut pixmap,
            Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
            Rgb(1, 2, 3),
        );
        fill_rect(
            &mut pixmap,
            Rect::from_xywh(1.0, 0.0, 1.0, 1.0),
            Rgb(4, 5, 6),
        );

        assert_eq!(rotated_rgb(&pixmap), [4, 5, 6, 1, 2, 3]);
    }

    #[test]
    fn key_image_is_a_jpeg_image() {
        let jpeg = key(&fonts(), &view(Tone::Plane(PlaneTone::Live), "ON AIR")).unwrap();

        assert_eq!(jpeg[..2], [0xff, 0xd8]);
    }

    #[test]
    fn blend_at_half_coverage_gives_the_middle() {
        assert_eq!(blend(0, 200, 0.5), 100);
    }

    #[test]
    fn missing_font_is_a_read_error() {
        let result = Fonts::load(Path::new("/nonexistent"));

        assert!(matches!(result, Err(FontError::Read { .. })));
    }
}
