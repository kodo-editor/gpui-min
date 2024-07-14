use crate::{
    point, size, Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun, FontStyle,
    FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, Point, RenderGlyphParams,
    ShapedGlyph, SharedString, Size,
};
use anyhow::{anyhow, Context, Ok, Result};
use cosmic_text::{
    Attrs, AttrsList, CacheKey, Family, Font as CosmicTextFont, FontSystem, ShapeBuffer, ShapeLine,
    SwashCache,
};

use itertools::Itertools;
use parking_lot::RwLock;
use pathfinder_geometry::{
    rect::{RectF, RectI},
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, collections::HashMap, path::PathBuf, sync::Arc};

pub(crate) struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

struct CosmicTextSystemState {
    swash_cache: SwashCache,
    font_system: FontSystem,
    scratch: ShapeBuffer,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts_store: Vec<Arc<CosmicTextFont>>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<SharedString, SmallVec<[FontId; 4]>>,
    /// The name of each font associated with the given font id
    postscript_names: HashMap<FontId, String>,
}

impl CosmicTextSystem {
    pub(crate) fn new() -> Self {
        let mut font_system = FontSystem::new();

        // todo(linux) make font loading non-blocking
        font_system.db_mut().load_system_fonts();

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            swash_cache: SwashCache::new(),
            scratch: ShapeBuffer::default(),
            loaded_fonts_store: Vec::new(),
            font_ids_by_family_cache: HashMap::default(),
            postscript_names: HashMap::default(),
        }))
    }
}

impl Default for CosmicTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.0
            .read()
            .font_system
            .db()
            .faces()
            .map(|face| face.post_script_name.clone())
            .collect()
    }

    fn all_font_families(&self) -> Vec<String> {
        self.0
            .read()
            .font_system
            .db()
            .faces()
            // todo(linux) this will list the same font family multiple times
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec()
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        // todo(linux): Do we need to use CosmicText's Font APIs? Can we consolidate this to use font_kit?
        let mut state = self.0.write();

        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&font.family) {
            font_ids.as_slice()
        } else {
            let font_ids = state.load_family(&font.family, &font.features)?;
            state
                .font_ids_by_family_cache
                .insert(font.family.clone(), font_ids);
            state.font_ids_by_family_cache[&font.family].as_ref()
        };

        // todo(linux) ideally we would make fontdb's `find_best_match` pub instead of using font-kit here
        let candidate_properties = candidates
            .iter()
            .map(|font_id| {
                let database_id = state.loaded_fonts_store[font_id.0].id();
                let face_info = state.font_system.db().face(database_id).expect("");
                face_info.clone()
            })
            .collect::<SmallVec<[_; 4]>>();

        let ix = find_best_match(&candidate_properties, &font.into())
            .context("requested font family contains no font matching the other parameters")?;

        Ok(candidates[ix])
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self.0.read().loaded_fonts_store[font_id.0]
            .as_swash()
            .metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent, // todo(linux) confirm this is correct
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            // todo(linux): Compute this correctly
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let glyph_metrics = lock.loaded_fonts_store[font_id.0]
            .as_swash()
            .glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        // todo(linux): Compute this correctly
        // see https://github.com/servo/font-kit/blob/master/src/loaders/freetype.rs#L614-L620
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }
}

impl CosmicTextSystemState {
    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    db.load_font_data(embedded_font.to_vec());
                }
                Cow::Owned(bytes) => {
                    db.load_font_data(bytes);
                }
            }
        }
        Ok(())
    }

    // todo(linux) handle `FontFeatures`
    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        _features: &FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        // TODO: Determine the proper system UI font.
        let name = if name == ".SystemUIFont" {
            "Zed Plex Sans"
        } else {
            name
        };

        let mut font_ids = SmallVec::new();
        let families = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| *name == family.0))
            .map(|face| (face.id, face.post_script_name.clone()))
            .collect::<SmallVec<[_; 4]>>();

        for (font_id, postscript_name) in families {
            let font = self
                .font_system
                .get_font(font_id)
                .ok_or_else(|| anyhow!("Could not load font"))?;

            // HACK: To let the storybook run and render Windows caption icons. We should actually do better font fallback.
            let allowed_bad_font_names = [
                "SegoeFluentIcons", // NOTE: Segoe fluent icons postscript name is inconsistent
                "Segoe Fluent Icons",
            ];

            if font.as_swash().charmap().map('m') == 0
                && !allowed_bad_font_names.contains(&postscript_name.as_str())
            {
                self.font_system.db_mut().remove_face(font.id());
                continue;
            };

            let font_id = FontId(self.loaded_fonts_store.len());
            font_ids.push(font_id);
            self.loaded_fonts_store.push(font);
            self.postscript_names.insert(font_id, postscript_name);
        }

        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let width = self.loaded_fonts_store[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_width(glyph_id.0 as u16);
        let height = self.loaded_fonts_store[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_height(glyph_id.0 as u16);
        Ok(Size { width, height })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.loaded_fonts_store[font_id.0]
            .as_swash()
            .charmap()
            .map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        // TODO: Include other common emoji fonts
        self.postscript_names
            .get(&font_id)
            .map_or(false, |postscript_name| postscript_name == "NotoColorEmoji")
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.loaded_fonts_store[params.font_id.0];
        let font_system = &mut self.font_system;
        let image = self
            .swash_cache
            .get_image(
                font_system,
                CacheKey::new(
                    font.id(),
                    params.glyph_id.0 as u16,
                    (params.font_size * params.scale_factor).into(),
                    (0.0, 0.0),
                    cosmic_text::CacheKeyFlags::empty(),
                )
                .0,
            )
            .clone()
            .with_context(|| format!("no image for {params:?} in font {font:?}"))?;
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            Err(anyhow!("glyph bounds are empty"))
        } else {
            // todo(linux) handle subpixel variants
            let bitmap_size = glyph_bounds.size;
            let font = &self.loaded_fonts_store[params.font_id.0];
            let font_system = &mut self.font_system;
            let mut image = self
                .swash_cache
                .get_image(
                    font_system,
                    CacheKey::new(
                        font.id(),
                        params.glyph_id.0 as u16,
                        (params.font_size * params.scale_factor).into(),
                        (0.0, 0.0),
                        cosmic_text::CacheKeyFlags::empty(),
                    )
                    .0,
                )
                .clone()
                .with_context(|| format!("no image for {params:?} in font {font:?}"))?;

            if params.is_emoji {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }

            Ok((bitmap_size, image.data))
        }
    }

    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> FontId {
        if let Some(ix) = self
            .loaded_fonts_store
            .iter()
            .position(|font| font.id() == id)
        {
            FontId(ix)
        } else {
            // This matches the behavior of the mac text system
            let font = self.font_system.get_font(id).unwrap();
            let face = self
                .font_system
                .db()
                .faces()
                .find(|info| info.id == id)
                .unwrap();

            let font_id = FontId(self.loaded_fonts_store.len());
            self.loaded_fonts_store.push(font);
            self.postscript_names
                .insert(font_id, face.post_script_name.clone());

            font_id
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let mut attrs_list = AttrsList::new(Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let font = &self.loaded_fonts_store[run.font_id.0];
            let font = self.font_system.db().face(font.id()).unwrap();
            attrs_list.add_span(
                offs..(offs + run.len),
                Attrs::new()
                    .family(Family::Name(&font.families.first().unwrap().0))
                    .stretch(font.stretch)
                    .style(font.style)
                    .weight(font.weight),
            );
            offs += run.len;
        }
        let mut line = ShapeLine::new_in_buffer(
            &mut self.scratch,
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );

        let mut layout = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            font_size.0,
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            None,
            &mut layout,
            None,
        );

        let mut runs = Vec::new();
        let layout = layout.first().unwrap();
        for glyph in &layout.glyphs {
            let font_id = glyph.font_id;
            let font_id = self.font_id_for_cosmic_id(font_id);
            let is_emoji = self.is_emoji(font_id);
            let mut glyphs = SmallVec::new();

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && is_emoji {
                continue;
            }

            // todo(linux) this is definitely wrong, each glyph in glyphs from cosmic-text is a cluster with one glyph, ShapedRun takes a run of glyphs with the same font and direction
            glyphs.push(ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            });

            runs.push(crate::ShapedRun { font_id, glyphs });
        }

        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for cosmic_text::Weight {
    fn from(value: FontWeight) -> Self {
        cosmic_text::Weight(value.0 as u16)
    }
}

impl From<FontStyle> for cosmic_text::Style {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => cosmic_text::Style::Normal,
            FontStyle::Italic => cosmic_text::Style::Italic,
            FontStyle::Oblique => cosmic_text::Style::Oblique,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("no matching font could be found")]
struct MatchNotFound;

fn stretch_to_num(stretch: cosmic_text::Stretch) -> u32 {
    match stretch {
        cosmic_text::Stretch::UltraCondensed => 100,
        cosmic_text::Stretch::ExtraCondensed => 101,
        cosmic_text::Stretch::Condensed => 102,
        cosmic_text::Stretch::SemiCondensed => 103,
        cosmic_text::Stretch::Normal => 104,
        cosmic_text::Stretch::SemiExpanded => 105,
        cosmic_text::Stretch::Expanded => 107,
        cosmic_text::Stretch::ExtraExpanded => 108,
        cosmic_text::Stretch::UltraExpanded => 109,
    }
}

fn find_best_match(
    candidates: &[cosmic_text::fontdb::FaceInfo],
    query: &cosmic_text::fontdb::FaceInfo,
) -> Result<usize, MatchNotFound> {
    // Step 4.
    let mut matching_set: Vec<usize> = (0..candidates.len()).collect();
    if matching_set.is_empty() {
        return Err(MatchNotFound);
    }

    // Step 4a (`font-stretch`).
    let matching_stretch = if matching_set
        .iter()
        .any(|&index| candidates[index].stretch == query.stretch)
    {
        // Exact match.
        query.stretch
    } else if query.stretch <= cosmic_text::Stretch::Normal {
        // Closest width, first checking narrower values and then wider values.
        match matching_set
            .iter()
            .filter(|&&index| candidates[index].stretch < query.stretch)
            .min_by_key(|&&index| {
                stretch_to_num(query.stretch) - stretch_to_num(candidates[index].stretch)
            }) {
            Some(&matching_index) => candidates[matching_index].stretch,
            None => {
                let matching_index = *matching_set
                    .iter()
                    .min_by_key(|&&index| {
                        stretch_to_num(candidates[index].stretch) - stretch_to_num(query.stretch)
                    })
                    .unwrap();
                candidates[matching_index].stretch
            }
        }
    } else {
        // Closest width, first checking wider values and then narrower values.
        match matching_set
            .iter()
            .filter(|&&index| candidates[index].stretch > query.stretch)
            .min_by_key(|&&index| {
                stretch_to_num(candidates[index].stretch) - stretch_to_num(query.stretch)
            }) {
            Some(&matching_index) => candidates[matching_index].stretch,
            None => {
                let matching_index = *matching_set
                    .iter()
                    .min_by_key(|&&index| {
                        stretch_to_num(query.stretch) - stretch_to_num(candidates[index].stretch)
                    })
                    .unwrap();
                candidates[matching_index].stretch
            }
        }
    };
    matching_set.retain(|&index| candidates[index].stretch == matching_stretch);

    // Step 4b (`font-style`).
    let style_preference = match query.style {
        cosmic_text::Style::Italic => [
            cosmic_text::Style::Italic,
            cosmic_text::Style::Oblique,
            cosmic_text::Style::Normal,
        ],
        cosmic_text::Style::Oblique => [
            cosmic_text::Style::Oblique,
            cosmic_text::Style::Italic,
            cosmic_text::Style::Normal,
        ],
        cosmic_text::Style::Normal => [
            cosmic_text::Style::Normal,
            cosmic_text::Style::Oblique,
            cosmic_text::Style::Italic,
        ],
    };
    let matching_style = *style_preference
        .iter()
        .find(|&query_style| {
            matching_set
                .iter()
                .any(|&index| candidates[index].style == *query_style)
        })
        .unwrap();
    matching_set.retain(|&index| candidates[index].style == matching_style);

    // Step 4c (`font-weight`).
    //
    // The spec doesn't say what to do if the weight is between 400 and 500 exclusive, so we
    // just use 450 as the cutoff.
    let matching_weight = if matching_set
        .iter()
        .any(|&index| candidates[index].weight == query.weight)
    {
        query.weight
    } else if query.weight >= cosmic_text::Weight(400)
        && query.weight < cosmic_text::Weight(450)
        && matching_set
            .iter()
            .any(|&index| candidates[index].weight == cosmic_text::Weight(500))
    {
        // Check 500 first.
        cosmic_text::Weight(500)
    } else if query.weight >= cosmic_text::Weight(450)
        && query.weight <= cosmic_text::Weight(500)
        && matching_set
            .iter()
            .any(|&index| candidates[index].weight == cosmic_text::Weight(400))
    {
        // Check 400 first.
        cosmic_text::Weight(400)
    } else if query.weight <= cosmic_text::Weight(500) {
        // Closest weight, first checking thinner values and then fatter ones.
        match matching_set
            .iter()
            .filter(|&&index| candidates[index].weight <= query.weight)
            .min_by_key(|&&index| query.weight.0 - candidates[index].weight.0)
        {
            Some(&matching_index) => candidates[matching_index].weight,
            None => {
                let matching_index = *matching_set
                    .iter()
                    .min_by_key(|&&index| (candidates[index].weight.0 - query.weight.0))
                    .unwrap();
                candidates[matching_index].weight
            }
        }
    } else {
        // Closest weight, first checking fatter values and then thinner ones.
        match matching_set
            .iter()
            .filter(|&&index| candidates[index].weight >= query.weight)
            .min_by_key(|&&index| (candidates[index].weight.0 - query.weight.0))
        {
            Some(&matching_index) => candidates[matching_index].weight,
            None => {
                let matching_index = *matching_set
                    .iter()
                    .min_by_key(|&&index| (query.weight.0 - candidates[index].weight.0))
                    .unwrap();
                candidates[matching_index].weight
            }
        }
    };
    matching_set.retain(|&index| candidates[index].weight == matching_weight);

    // Step 4d concerns `font-size`, but fonts in `font-kit` are unsized, so we ignore that.

    // Return the result.
    matching_set.into_iter().next().ok_or(MatchNotFound)
}

impl From<&Font> for cosmic_text::fontdb::FaceInfo {
    fn from(value: &Font) -> Self {
        cosmic_text::fontdb::FaceInfo {
            id: cosmic_text::fontdb::ID::dummy(),
            source: cosmic_text::fontdb::Source::File(PathBuf::new()),
            index: 0,
            families: Vec::new(),
            post_script_name: String::new(),
            style: match value.style {
                FontStyle::Normal => cosmic_text::Style::Normal,
                FontStyle::Italic => cosmic_text::Style::Italic,
                FontStyle::Oblique => cosmic_text::Style::Oblique,
            },
            weight: cosmic_text::Weight(value.weight.0.round() as u16),
            stretch: cosmic_text::Stretch::Normal,
            monospaced: true,
        }
    }
}
