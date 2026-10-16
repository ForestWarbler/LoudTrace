use super::*;
use skrifa::outline::{DrawSettings, pen::PathElement};

fn has_drawable_outline(font: &FontRef<'_>, c: char) -> bool {
    let Some(glyph) = font.charmap().map(c).filter(|g| g.to_u32() != 0) else {
        return false;
    };
    let Some(outline) = font.outline_glyphs().get(glyph) else {
        return false;
    };
    let mut path = Vec::<PathElement>::new();
    outline
        .draw(
            DrawSettings::unhinted(
                skrifa::instance::Size::new(16.0),
                skrifa::instance::LocationRef::default(),
            ),
            &mut path,
        )
        .is_ok()
        && path.iter().any(|element| {
            matches!(
                element,
                PathElement::LineTo { .. }
                    | PathElement::QuadTo { .. }
                    | PathElement::CurveTo { .. }
            )
        })
}

fn assert_visible_in_chain(definitions: &FontDefinitions, family: &FontFamily, c: char) {
    // Egui stops at the first cmap claimant, even when that font has only a
    // bitmap or an empty dummy outline. Looking for any later valid font would
    // miss precisely the blank-glyph regression this test guards against.
    let claimant = definitions.families[family].iter().find(|name| {
        let data = &definitions.font_data[*name];
        let font = FontRef::from_index(&data.font, data.index).unwrap();
        font.charmap().map(c).is_some_and(|g| g.to_u32() != 0)
    });
    let name = claimant.unwrap_or_else(|| panic!("missing {c:?} in {family:?}"));
    let data = &definitions.font_data[name];
    let font = FontRef::from_index(&data.font, data.index).unwrap();
    assert!(
        has_drawable_outline(&font, c),
        "{name} claims {c:?} in {family:?} but draws no visible outline"
    );
}

#[test]
fn no_embedded_fonts_and_empty_system_reports_an_actionable_error() {
    assert!(
        FontDefinitions::default().font_data.is_empty(),
        "default_fonts was re-enabled"
    );
    assert!(
        definitions(&Database::new())
            .unwrap_err()
            .contains("Install")
    );
}

#[test]
fn malformed_preferred_font_is_skipped_for_a_valid_system_face() {
    let installed = load().expect("a desktop UI font must be installed");
    let source = &installed.font_data[&installed.families[&FontFamily::Proportional][0]];
    let mut source_database = Database::new();
    source_database.load_font_data(source.font.to_vec());
    let mut valid = source_database
        .faces()
        .find(|face| face.index == source.index)
        .unwrap()
        .clone();
    let mut broken = valid.clone();
    broken.source = fontdb::Source::Binary(Arc::new(vec![0_u8; 16]));
    broken.post_script_name = "000-MalformedPreferredFont".into();
    broken.families[0].0 = UI_FAMILIES[0].into();
    valid.post_script_name = "TestUsableSystemFont".into();
    // A damaged regular face must not hide another usable face of its family.
    valid.families[0].0 = UI_FAMILIES[0].into();
    let mut database = Database::new();
    database.push_face_info(broken);
    database.push_face_info(valid);

    let definitions = definitions(&database).expect("the valid fallback should remain usable");
    assert!(
        definitions
            .font_data
            .keys()
            .all(|name| !name.contains("Malformed"))
    );
    assert_visible_in_chain(&definitions, &FontFamily::Proportional, 'L');
}

#[test]
fn collection_face_index_is_preserved() {
    let installed = load().expect("a desktop UI font must be installed");
    let source = &installed.font_data[&installed.families[&FontFamily::Proportional][0]];
    let bytes = source.font.as_ref();
    let read_u32 =
        |offset: usize| u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    let directory = if bytes.starts_with(b"ttcf") {
        read_u32(12 + source.index as usize * 4)
    } else {
        0
    };

    // Wrap an installed sfnt face in an in-memory TTC with two references to
    // its directory. No font fixture or copied system font is saved to disk.
    let mut collection = Vec::with_capacity(bytes.len() + 20);
    collection.extend_from_slice(b"ttcf\0\x01\0\0\0\0\0\x02");
    collection.extend_from_slice(&((directory + 20) as u32).to_be_bytes());
    collection.extend_from_slice(&((directory + 20) as u32).to_be_bytes());
    collection.extend_from_slice(bytes);
    let table_count =
        u16::from_be_bytes(bytes[directory + 4..directory + 6].try_into().unwrap()) as usize;
    for table in 0..table_count {
        let old_offset = directory + 12 + table * 16 + 8;
        let new_offset = old_offset + 20;
        collection[new_offset..new_offset + 4]
            .copy_from_slice(&((read_u32(old_offset) + 20) as u32).to_be_bytes());
    }
    let mut database = Database::new();
    database.load_font_data(collection);
    assert_eq!(database.len(), 2);
    let first = database.faces().find(|face| face.index == 0).unwrap().id;
    database.remove_face(first);

    let definitions = definitions(&database).expect("collection face 1 must load");
    assert!(definitions.font_data.values().all(|data| data.index == 1));
    assert_visible_in_chain(&definitions, &FontFamily::Proportional, 'L');
}

#[test]
fn installed_fonts_draw_ui_and_available_cjk_and_symbols() {
    let definitions = load().expect("a desktop UI font must be installed");
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        for c in "LoudTrace0123456789−14.0·−1dBTP…→×—’".chars() {
            assert_visible_in_chain(&definitions, &family, c);
        }
    }

    let mut database = Database::new();
    database.load_system_fonts();
    for (families, sample) in [(CJK_FAMILIES, CJK_SAMPLE), (SYMBOL_FAMILIES, SYMBOL_SAMPLE)] {
        let mut available = HashSet::new();
        for name in families {
            let Some(id) = database.query(&Query {
                families: &[Family::Name(name)],
                ..Default::default()
            }) else {
                continue;
            };
            database.with_face_data(id, |bytes, index| {
                if let Ok(font) = FontRef::from_index(bytes, index) {
                    available.extend(sample.chars().filter(|c| has_drawable_outline(&font, *c)));
                }
            });
        }
        for c in available {
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                assert_visible_in_chain(&definitions, &family, c);
            }
        }
    }

    let ctx = eframe::egui::Context::default();
    ctx.set_fonts(definitions.clone());
    ctx.run_ui(Default::default(), |ui| {
        ui.label("LoudTrace −14.0 LUFS · 中文繁體 … →");
        ui.fonts_mut(|fonts| {
            let id = eframe::egui::FontId::monospace(14.0);
            let width = fonts.glyph_width(&id, '0');
            for c in "123456789iMW.,+-".chars() {
                assert!(
                    (fonts.glyph_width(&id, c) - width).abs() < 0.01,
                    "not monospace: {c}"
                );
            }
        });
    })
    .drop_without_applying_deltas();
    eprintln!("System font chains: {:?}", definitions.families);
}
