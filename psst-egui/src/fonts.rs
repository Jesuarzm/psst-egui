use eframe::egui;

const CJK_FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
];

pub fn install_cjk_font(ctx: &egui::Context) {
    let Some(bytes) = CJK_FONT_PATHS.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();

    let mut font_data = egui::FontData::from_owned(bytes);
    // índice 0 del .ttc es "Noto Sans CJK JP" (confirmado con `fc-scan`)
    font_data.index = 0;
    fonts.font_data.insert("cjk".to_owned(), font_data);

    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .push("cjk".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .push("cjk".to_owned());

    ctx.set_fonts(fonts);
}
