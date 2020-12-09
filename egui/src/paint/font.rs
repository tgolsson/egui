use std::sync::Arc;

use {
    ahash::AHashMap,
    parking_lot::RwLock,
    rusttype::{point, Scale},
};

use crate::{
    math::{vec2, Vec2},
    mutex::Mutex,
    paint::{Galley, Row},
};

use super::texture_atlas::TextureAtlas;

// ----------------------------------------------------------------------------

// const REPLACEMENT_CHAR: char = '\u{25A1}'; // □ white square Replaces a missing or unsupported Unicode character.
// const REPLACEMENT_CHAR: char = '\u{FFFD}'; // � REPLACEMENT CHARACTER
const REPLACEMENT_CHAR: char = '?';

#[derive(Clone, Copy, Debug)]
pub struct UvRect {
    /// X/Y offset for nice rendering (unit: points).
    pub offset: Vec2,
    pub size: Vec2,

    /// Top left corner UV in texture.
    pub min: (u16, u16),

    /// Bottom right corner (exclusive).
    pub max: (u16, u16),
}

#[derive(Clone, Copy, Debug)]
pub struct GlyphInfo {
    id: rusttype::GlyphId,

    /// Unit: points.
    pub advance_width: f32,

    /// Texture coordinates. None for space.
    pub uv_rect: Option<UvRect>,
}

/// The interface uses points as the unit for everything.
pub struct Font {
    font: rusttype::Font<'static>,
    /// Maximum character height
    scale_in_pixels: f32,
    pixels_per_point: f32,
    replacement_glyph_info: GlyphInfo,
    glyph_infos: RwLock<AHashMap<char, GlyphInfo>>,
    atlas: Arc<Mutex<TextureAtlas>>,
}

impl Font {
    pub fn new(
        atlas: Arc<Mutex<TextureAtlas>>,
        font_data: &'static [u8],
        scale_in_points: f32,
        pixels_per_point: f32,
    ) -> Font {
        assert!(scale_in_points > 0.0);
        assert!(pixels_per_point > 0.0);

        let font = rusttype::Font::try_from_bytes(font_data).expect("Error constructing Font");
        let scale_in_pixels = pixels_per_point * scale_in_points;

        let replacement_glyph_info = allocate_glyph(
            &mut atlas.lock(),
            REPLACEMENT_CHAR,
            &font,
            scale_in_pixels,
            pixels_per_point,
        )
        .unwrap_or_else(|| {
            panic!(
                "Failed to find replacement character {:?}",
                REPLACEMENT_CHAR
            )
        });

        let font = Font {
            font,
            scale_in_pixels,
            pixels_per_point,
            replacement_glyph_info,
            glyph_infos: Default::default(),
            atlas,
        };

        font.glyph_infos
            .write()
            .insert(REPLACEMENT_CHAR, font.replacement_glyph_info);

        // Preload the printable ASCII characters [32, 126] (which excludes control codes):
        const FIRST_ASCII: usize = 32; // 32 == space
        const LAST_ASCII: usize = 126;
        for c in (FIRST_ASCII..=LAST_ASCII).map(|c| c as u8 as char) {
            font.glyph_info(c);
        }
        font.glyph_info('°');

        font
    }

    pub fn round_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).round() / self.pixels_per_point
    }

    /// Height of one row of text. In points
    pub fn row_height(&self) -> f32 {
        self.scale_in_pixels / self.pixels_per_point
    }

    pub fn uv_rect(&self, c: char) -> Option<UvRect> {
        self.glyph_infos.read().get(&c).and_then(|gi| gi.uv_rect)
    }

    pub fn glyph_width(&self, c: char) -> f32 {
        self.glyph_info(c).advance_width
    }

    /// `\n` will (intentionally) show up as '?' (`REPLACEMENT_CHAR`)
    fn glyph_info(&self, c: char) -> GlyphInfo {
        {
            if let Some(glyph_info) = self.glyph_infos.read().get(&c) {
                return *glyph_info;
            }
        }

        // Add new character:
        let glyph_info = allocate_glyph(
            &mut self.atlas.lock(),
            c,
            &self.font,
            self.scale_in_pixels,
            self.pixels_per_point,
        );
        // debug_assert!(glyph_info.is_some(), "Failed to find {:?}", c);
        let glyph_info = glyph_info.unwrap_or(self.replacement_glyph_info);
        self.glyph_infos.write().insert(c, glyph_info);
        glyph_info
    }

    /// Typeset the given text onto one row.
    /// Any `\n` will show up as `REPLACEMENT_CHAR` ('?').
    /// Always returns exactly one `Row` in the `Galley`.
    pub fn layout_single_line(&self, text: String) -> Galley {
        let x_offsets = self.layout_single_row_fragment(&text);
        let row = Row {
            x_offsets,
            y_min: 0.0,
            y_max: self.row_height(),
            ends_with_newline: false,
        };
        let width = row.max_x();
        let size = vec2(width, self.row_height());
        let galley = Galley {
            text,
            rows: vec![row],
            size,
        };
        galley.sanity_check();
        galley
    }

    /// Always returns at least one row.
    pub fn layout_multiline(&self, text: String, max_width_in_points: f32) -> Galley {
        self.layout_multiline_with_indentation_and_max_width(text, 0.0, max_width_in_points)
    }

    /// * `first_row_indentation`: extra space before the very first character (in points).
    /// * `max_width_in_points`: wrapping width.
    /// Always returns at least one row.
    pub fn layout_multiline_with_indentation_and_max_width(
        &self,
        text: String,
        first_row_indentation: f32,
        max_width_in_points: f32,
    ) -> Galley {
        let row_height = self.row_height();
        let mut cursor_y = 0.0;
        let mut rows = Vec::new();

        let mut paragraph_start = 0;

        while paragraph_start < text.len() {
            let next_newline = text[paragraph_start..].find('\n');
            let paragraph_end = next_newline
                .map(|newline| paragraph_start + newline)
                .unwrap_or_else(|| text.len());

            assert!(paragraph_start <= paragraph_end);
            let paragraph_text = &text[paragraph_start..paragraph_end];
            let line_indentation = if rows.is_empty() {
                first_row_indentation
            } else {
                0.0
            };
            let mut paragraph_rows = self.layout_paragraph_max_width(
                paragraph_text,
                line_indentation,
                max_width_in_points,
            );
            assert!(!paragraph_rows.is_empty());
            paragraph_rows.last_mut().unwrap().ends_with_newline = next_newline.is_some();

            for row in &mut paragraph_rows {
                row.y_min += cursor_y;
                row.y_max += cursor_y;
            }
            cursor_y = paragraph_rows.last().unwrap().y_max;
            cursor_y += row_height * 0.4; // Extra spacing between paragraphs. TODO: less hacky

            rows.append(&mut paragraph_rows);

            paragraph_start = paragraph_end + 1;
        }

        if text.is_empty() || text.ends_with('\n') {
            rows.push(Row {
                x_offsets: vec![0.0],
                y_min: cursor_y,
                y_max: cursor_y + row_height,
                ends_with_newline: false,
            });
        }

        let mut widest_row = 0.0;
        for row in &rows {
            widest_row = row.max_x().max(widest_row);
        }
        let size = vec2(widest_row, rows.last().unwrap().y_max);

        let galley = Galley { text, rows, size };
        galley.sanity_check();
        galley
    }

    /// Typeset the given text onto one row.
    /// Assumes there are no `\n` in the text.
    /// Return `x_offsets`, one longer than the number of characters in the text.
    fn layout_single_row_fragment(&self, text: &str) -> Vec<f32> {
        let scale_in_pixels = Scale::uniform(self.scale_in_pixels);

        let mut x_offsets = Vec::with_capacity(text.chars().count() + 1);
        x_offsets.push(0.0);

        let mut cursor_x_in_points = 0.0f32;
        let mut last_glyph_id = None;

        for c in text.chars() {
            let glyph = self.glyph_info(c);

            if let Some(last_glyph_id) = last_glyph_id {
                cursor_x_in_points +=
                    self.font
                        .pair_kerning(scale_in_pixels, last_glyph_id, glyph.id)
                        / self.pixels_per_point
            }
            cursor_x_in_points += glyph.advance_width;
            cursor_x_in_points = self.round_to_pixel(cursor_x_in_points);
            last_glyph_id = Some(glyph.id);

            x_offsets.push(cursor_x_in_points);
        }

        x_offsets
    }

    /// A paragraph is text with no line break character in it.
    /// The text will be wrapped by the given `max_width_in_points`.
    /// Always returns at least one row.
    fn layout_paragraph_max_width(
        &self,
        text: &str,
        mut first_row_indentation: f32,
        max_width_in_points: f32,
    ) -> Vec<Row> {
        if text == "" {
            return vec![Row {
                x_offsets: vec![first_row_indentation],
                y_min: 0.0,
                y_max: self.row_height(),
                ends_with_newline: false,
            }];
        }

        let full_x_offsets = self.layout_single_row_fragment(text);

        let mut row_start_x = 0.0; // NOTE: BEFORE the `first_row_indentation`.

        let mut cursor_y = 0.0;
        let mut row_start_idx = 0;

        // start index of the last space. A candidate for a new row.
        let mut last_space = None;

        let mut out_rows = vec![];

        for (i, (x, chr)) in full_x_offsets.iter().skip(1).zip(text.chars()).enumerate() {
            debug_assert!(chr != '\n');
            let potential_row_width = first_row_indentation + x - row_start_x;

            if potential_row_width > max_width_in_points {
                if let Some(last_space_idx) = last_space {
                    // We include the trailing space in the row:
                    let row = Row {
                        x_offsets: full_x_offsets[row_start_idx..=last_space_idx + 1]
                            .iter()
                            .map(|x| first_row_indentation + x - row_start_x)
                            .collect(),
                        y_min: cursor_y,
                        y_max: cursor_y + self.row_height(),
                        ends_with_newline: false,
                    };
                    row.sanity_check();
                    out_rows.push(row);

                    row_start_idx = last_space_idx + 1;
                    row_start_x = first_row_indentation + full_x_offsets[row_start_idx];
                    last_space = None;
                    cursor_y = self.round_to_pixel(cursor_y + self.row_height());
                } else if out_rows.is_empty() && first_row_indentation > 0.0 {
                    assert_eq!(row_start_idx, 0);
                    // Allow the first row to be completely empty, because we know there will be more space on the next row:
                    let row = Row {
                        x_offsets: vec![first_row_indentation],
                        y_min: cursor_y,
                        y_max: cursor_y + self.row_height(),
                        ends_with_newline: false,
                    };
                    row.sanity_check();
                    out_rows.push(row);
                    cursor_y = self.round_to_pixel(cursor_y + self.row_height());
                    first_row_indentation = 0.0; // Continue all other rows as if there is no indentation
                }
            }

            const NON_BREAKING_SPACE: char = '\u{A0}';
            if chr.is_whitespace() && chr != NON_BREAKING_SPACE {
                last_space = Some(i);
            }
        }

        if row_start_idx + 1 < full_x_offsets.len() {
            let row = Row {
                x_offsets: full_x_offsets[row_start_idx..]
                    .iter()
                    .map(|x| first_row_indentation + x - row_start_x)
                    .collect(),
                y_min: cursor_y,
                y_max: cursor_y + self.row_height(),
                ends_with_newline: false,
            };
            row.sanity_check();
            out_rows.push(row);
        }

        out_rows
    }
}

fn allocate_glyph(
    atlas: &mut TextureAtlas,
    c: char,
    font: &rusttype::Font<'static>,
    scale_in_pixels: f32,
    pixels_per_point: f32,
) -> Option<GlyphInfo> {
    let glyph = font.glyph(c);
    if glyph.id().0 == 0 {
        return None; // Failed to find a glyph for the character
    }

    let glyph = glyph.scaled(Scale::uniform(scale_in_pixels));
    let glyph = glyph.positioned(point(0.0, 0.0));

    let uv_rect = if let Some(bb) = glyph.pixel_bounding_box() {
        let glyph_width = bb.width() as usize;
        let glyph_height = bb.height() as usize;
        assert!(glyph_width >= 1);
        assert!(glyph_height >= 1);

        let glyph_pos = atlas.allocate((glyph_width, glyph_height));

        let texture = atlas.texture_mut();
        glyph.draw(|x, y, v| {
            if v > 0.0 {
                let px = glyph_pos.0 + x as usize;
                let py = glyph_pos.1 + y as usize;
                texture[(px, py)] = (v * 255.0).round() as u8;
            }
        });

        let offset_y_in_pixels = scale_in_pixels as f32 + bb.min.y as f32 - 4.0 * pixels_per_point; // TODO: use font.v_metrics
        Some(UvRect {
            offset: vec2(
                bb.min.x as f32 / pixels_per_point,
                offset_y_in_pixels / pixels_per_point,
            ),
            size: vec2(glyph_width as f32, glyph_height as f32) / pixels_per_point,
            min: (glyph_pos.0 as u16, glyph_pos.1 as u16),
            max: (
                (glyph_pos.0 + glyph_width) as u16,
                (glyph_pos.1 + glyph_height) as u16,
            ),
        })
    } else {
        // No bounding box. Maybe a space?
        None
    };

    let advance_width_in_points = glyph.unpositioned().h_metrics().advance_width / pixels_per_point;

    Some(GlyphInfo {
        id: glyph.id(),
        advance_width: advance_width_in_points,
        uv_rect,
    })
}
