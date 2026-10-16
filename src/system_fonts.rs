//! Runtime-only system fonts. No font binaries are shipped with LoudTrace.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use fontdb::{Database, Family, ID, Query, Style};
use skrifa::{FontRef, MetadataProvider};

// Prefer the platform's UI families, but discover their files and collection
// indices instead of assuming paths (PingFang, for example, can live in AssetsV2).
#[cfg(target_os = "macos")]
const UI_FAMILIES: &[&str] = &[
    ".SF NS",
    "SF Pro Text",
    "SF Pro",
    "Helvetica Neue",
    "Helvetica",
    "Arial",
];
#[cfg(target_os = "windows")]
const UI_FAMILIES: &[&str] = &["Segoe UI Variable", "Segoe UI", "Tahoma", "Arial"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const UI_FAMILIES: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans", "FreeSans"];

#[cfg(target_os = "macos")]
const MONO_FAMILIES: &[&str] = &[".SF NS Mono", "SF Mono", "Menlo", "Monaco", "Courier"];
#[cfg(target_os = "windows")]
const MONO_FAMILIES: &[&str] = &["Cascadia Mono", "Consolas", "Lucida Console", "Courier New"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO_FAMILIES: &[&str] = &[
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "FreeMono",
];

const CJK_FAMILIES: &[&str] = &[
    "PingFang SC",
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "Noto Sans CJK SC",
    "Noto Sans SC",
    "Source Han Sans SC",
    "WenQuanYi Micro Hei",
    "Heiti SC",
    "PingFang TC",
    "Microsoft JhengHei UI",
    "Microsoft JhengHei",
    "Noto Sans CJK TC",
    "Noto Sans TC",
    "Source Han Sans TC",
    "Heiti TC",
    "SimSun",
    "Arial Unicode MS",
];
const SYMBOL_FAMILIES: &[&str] = &[
    "Apple Symbols",
    "Segoe UI Symbol",
    "Noto Sans Symbols 2",
    "Noto Sans Symbols",
    "DejaVu Sans",
    "Symbola",
    "Arial Unicode MS",
];
const CJK_SAMPLE: &str =
    "中文简体繁體漢字音乐音樂專輯专辑轨迹測試测试響度响度龍龙，。！？「」【】（）";
const SYMBOL_SAMPLE: &str = "‘’“”−–—…·•×÷±∞≤≥≠≈←→↑↓↔↕↗↘▶◀▸▾▴▷□■○●✓✔✕⚠ℹ⌘⌥⌃⏎⌫⏸⏹⏮⏭�";

static SYSTEM_FONTS: OnceLock<Result<FontDefinitions, String>> = OnceLock::new();

pub fn load() -> Result<FontDefinitions, String> {
    SYSTEM_FONTS
        .get_or_init(|| {
            let mut database = Database::new();
            database.load_system_fonts();
            definitions(&database)
        })
        .clone()
}

#[derive(Clone)]
struct Face {
    id: ID,
    name: String,
    coverage: HashSet<char>,
    fixed_width: bool,
}

type FaceCache = HashMap<ID, Option<Face>>;

fn inspect_face(database: &Database, cache: &mut FaceCache, id: ID) -> Option<Face> {
    cache
        .entry(id)
        .or_insert_with(|| read_face(database, id))
        .clone()
}

fn read_face(database: &Database, id: ID) -> Option<Face> {
    let info = database.face(id)?;
    // LastResort maps every Unicode block to a box; it must never mask a real
    // fallback. Legacy symbol encodings likewise aren't Unicode text fonts.
    if info
        .post_script_name
        .to_ascii_lowercase()
        .contains("lastresort")
    {
        return None;
    }
    database.with_face_data(id, |bytes, index| {
        let font = FontRef::from_index(bytes, index).ok()?;
        let charmap = font.charmap();
        let outlines = font.outline_glyphs();
        let color_font = [b"sbix", b"CBDT", b"COLR", b"SVG "]
            .iter()
            .any(|tag| font.table_data(skrifa::Tag::new(tag)).is_some());
        if color_font || !charmap.has_map() || charmap.is_symbol() || outlines.format().is_none() {
            // Egui renders outlines, not bitmap/color-only emoji fonts. Such
            // fonts can otherwise claim a character and render it as blank.
            return None;
        }
        // Inspect useful coverage, not every glyph of every installed font.
        // Egui can still use the selected font's entire cmap for actual text.
        let coverage = ('!'..='~')
            .chain(CJK_SAMPLE.chars())
            .chain(SYMBOL_SAMPLE.chars())
            .filter(|c| has_outline(&font, *c))
            .collect::<HashSet<_>>();
        if coverage.is_empty() {
            return None;
        }
        // Some system fonts omit the monospaced metadata. Verify advances too,
        // so proportional tabular digits alone cannot pass as a monospace face.
        let metrics = font.glyph_metrics(
            skrifa::instance::Size::unscaled(),
            skrifa::instance::LocationRef::default(),
        );
        let widths = "0123456789iMW.,+-"
            .chars()
            .map(|c| charmap.map(c).and_then(|g| metrics.advance_width(g)))
            .collect::<Option<Vec<_>>>();
        let fixed_width = widths.is_some_and(|widths| {
            widths[0] > 0.0 && widths.iter().all(|w| (w - widths[0]).abs() < 0.01)
        });
        Some(Face {
            id,
            name: format!("system:{}:{index}:{id:?}", info.post_script_name),
            coverage,
            fixed_width,
        })
    })?
}

fn has_outline(font: &FontRef<'_>, c: char) -> bool {
    let Some(id) = font.charmap().map(c).filter(|g| g.to_u32() != 0) else {
        return false;
    };
    let Some(outline) = font.outline_glyphs().get(id) else {
        return false;
    };
    let mut pen = OutlineProbe::default();
    outline
        .draw(skrifa::instance::Size::new(16.0), &mut pen)
        .is_ok()
        && pen.0
}

#[derive(Default)]
struct OutlineProbe(bool);

impl skrifa::outline::OutlinePen for OutlineProbe {
    fn move_to(&mut self, _: f32, _: f32) {}
    fn line_to(&mut self, _: f32, _: f32) {
        self.0 = true;
    }
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {
        self.0 = true;
    }
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
        self.0 = true;
    }
    fn close(&mut self) {}
}

fn materialize(database: &Database, id: ID) -> Option<Arc<FontData>> {
    database.with_face_data(id, |bytes, index| {
        FontRef::from_index(bytes, index).ok()?;
        let mut data = FontData::from_owned(bytes.to_vec());
        data.index = index;
        Some(Arc::new(data))
    })?
}

fn candidates(database: &Database, names: &[&str], generic: Option<Family<'_>>) -> Vec<ID> {
    let mut ids = Vec::new();
    let mut add = |family| {
        if let Some(id) = database.query(&Query {
            families: &[family],
            ..Default::default()
        }) && !ids.contains(&id)
        {
            ids.push(id);
        }
    };
    // On Linux, respect the generic aliases found in fontconfig before using
    // named alternatives. On macOS/Windows fontdb's generic defaults are not
    // the OS UI fonts, so the native family preferences go first.
    if cfg!(not(any(target_os = "macos", target_os = "windows")))
        && let Some(family) = generic
    {
        add(family);
    }
    for name in names {
        add(Family::Name(name));
    }
    if let Some(family) = generic {
        add(family);
    }

    // Unknown/custom font families remain usable. Try their regular faces
    // first; ordering is deterministic even when directory enumeration isn't.
    let mut remaining = database.faces().collect::<Vec<_>>();
    remaining.sort_by_key(|face| {
        (
            face.style != Style::Normal,
            face.weight.0.abs_diff(400),
            face.post_script_name.clone(),
            face.index,
        )
    });
    for face in remaining {
        if !ids.contains(&face.id) {
            ids.push(face.id);
        }
    }
    ids
}

fn primary(
    database: &Database,
    cache: &mut FaceCache,
    ids: &[ID],
    monospace: bool,
) -> Option<Face> {
    ids.iter()
        .filter_map(|id| inspect_face(database, cache, *id))
        .find(|face| {
            (!monospace || face.fixed_width) && ('!'..='~').all(|c| face.coverage.contains(&c))
        })
}

fn add_fallbacks(
    database: &Database,
    cache: &mut FaceCache,
    faces: &mut Vec<Face>,
    names: &[&str],
    sample: &str,
) {
    let mut missing = sample
        .chars()
        .filter(|c| !faces.iter().any(|f| f.coverage.contains(c)))
        .collect::<HashSet<_>>();
    if missing.is_empty() {
        return;
    }
    let ids = candidates(database, names, None);
    // Usually one CJK face covers both simplified and traditional Chinese. Do
    // not duplicate an entire (often 70+ MB) collection just to select two faces
    // that add no needed glyphs. Partial fonts can still complement each other.
    let mut considered = HashSet::new();
    while !missing.is_empty() {
        let mut best = None;
        let mut best_score = 0;
        let mut inspected_families = HashSet::new();
        for id in &ids {
            let Some(info) = database.face(*id) else {
                continue;
            };
            let family = info
                .families
                .first()
                .map(|f| f.0.as_str())
                .unwrap_or(&info.post_script_name);
            if inspected_families.contains(family) {
                continue;
            }
            // Only deduplicate a family after parsing a usable face. A broken
            // regular face must not hide a valid medium/bold face in that family.
            if cache.get(id).is_some_and(|face| face.is_some()) {
                inspected_families.insert(family);
            }
            if considered.contains(id) || faces.iter().any(|f| f.id == *id) {
                continue;
            }
            let Some(face) = inspect_face(database, cache, *id) else {
                considered.insert(*id);
                continue;
            };
            inspected_families.insert(family);
            let score = missing.intersection(&face.coverage).count();
            if score > best_score {
                best_score = score;
                best = Some(face);
                if score == missing.len() {
                    break;
                }
            } else if score == 0 {
                considered.insert(*id);
            }
        }
        let Some(face) = best else {
            break;
        };
        missing.retain(|c| !face.coverage.contains(c));
        considered.insert(face.id);
        faces.push(face);
    }
    if !missing.is_empty() {
        let mut chars = missing.into_iter().collect::<Vec<_>>();
        chars.sort_unstable();
        eprintln!(
            "Some characters have no outline font installed: {}",
            chars.into_iter().collect::<String>()
        );
    }
}

fn definitions(database: &Database) -> Result<FontDefinitions, String> {
    let mut cache = FaceCache::new();
    let ui = primary(database, &mut cache, &candidates(database, UI_FAMILIES, Some(Family::SansSerif)), false)
        .ok_or_else(|| "No usable system font was found. Install a TrueType or OpenType UI font (for example Noto Sans or DejaVu Sans), then restart LoudTrace.".to_string())?;
    let mono = primary(
        database,
        &mut cache,
        &candidates(database, MONO_FAMILIES, Some(Family::Monospace)),
        true,
    );
    let ui_name = ui.name.clone();
    let mono_name = mono
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_else(|| ui_name.clone());
    if mono.is_none() {
        eprintln!(
            "No monospaced system font found; using the UI font. Install a monospace font for aligned measurements."
        );
    }
    let mut faces = vec![ui];
    if let Some(mono) = mono
        && mono.id != faces[0].id
    {
        faces.push(mono);
    }
    add_fallbacks(database, &mut cache, &mut faces, CJK_FAMILIES, CJK_SAMPLE);
    add_fallbacks(
        database,
        &mut cache,
        &mut faces,
        SYMBOL_FAMILIES,
        SYMBOL_SAMPLE,
    );

    let mut definitions = FontDefinitions::empty();
    // All fixed-width faces precede proportional fallbacks in the monospace
    // chain. Both families share the same Arc, including large TTC files.
    let mut proportional = vec![ui_name];
    let mut monospace = vec![mono_name];
    for face in faces.iter().filter(|f| f.fixed_width) {
        if !monospace.contains(&face.name) {
            monospace.push(face.name.clone());
        }
    }
    for face in faces {
        if !proportional.contains(&face.name) {
            proportional.push(face.name.clone());
        }
        if !monospace.contains(&face.name) {
            monospace.push(face.name.clone());
        }
        let data = materialize(database, face.id).ok_or_else(|| format!("System font {} became unavailable. Restart LoudTrace to scan installed fonts again.", face.name))?;
        definitions.font_data.insert(face.name, data);
    }
    definitions
        .families
        .insert(FontFamily::Proportional, proportional);
    definitions
        .families
        .insert(FontFamily::Monospace, monospace);
    Ok(definitions)
}

#[cfg(test)]
mod tests;
