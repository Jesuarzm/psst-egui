//! La transparencia se pide al armar `NativeOptions` (`background_opacity()`), antes de que exista la ventana; el blur lo pone Hyprland.

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use eframe::egui::{self, Color32};

const KITTY_THEME_PATH: &str = ".config/kitty/themes/noctalia.conf";
const KITTY_CONF_PATH: &str = ".config/kitty/kitty.conf";
const DMS_COLORS_PATH: &str = ".cache/dms/dms-colors.json";
const GTK3_CSS_PATH: &str = ".config/gtk-3.0/gtk.css";
/// respaldo: DMS a veces solo escribe el gtk.css de GTK4.
const GTK4_CSS_PATH: &str = ".config/gtk-4.0/gtk.css";

const CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// archivos más recientes que esto pueden estar a medio escribir (DMS los reescribe).
const SETTLE_TIME: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeSource {
    Default,
    Dms,
    Gtk3,
}

impl ThemeSource {
    pub const ALL: [ThemeSource; 3] = [ThemeSource::Default, ThemeSource::Dms, ThemeSource::Gtk3];

    pub fn from_setting(value: &str) -> Self {
        match value {
            "dms" => ThemeSource::Dms,
            "gtk3" => ThemeSource::Gtk3,
            _ => ThemeSource::Default,
        }
    }

    pub fn as_setting(self) -> &'static str {
        match self {
            ThemeSource::Default => "default",
            ThemeSource::Dms => "dms",
            ThemeSource::Gtk3 => "gtk3",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeSource::Default => "Por defecto",
            ThemeSource::Dms => "DMS",
            ThemeSource::Gtk3 => "GTK3",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Palette {
    background: Color32,
    panel: Color32,
    text: Color32,
    text_muted: Color32,
    accent: Color32,
    on_accent: Color32,
    border: Color32,
    hover: Color32,
    selection: Color32,
    widget: Color32,
    widget_hover: Color32,
    error: Color32,
}

impl Default for Palette {
    fn default() -> Self {
        let accent = Color32::from_rgb(0x8a, 0xb4, 0xf8);
        let background = Color32::from_rgb(0x12, 0x12, 0x12);
        Self {
            background,
            panel: Color32::from_rgb(0x24, 0x24, 0x24),
            text: Color32::from_rgb(0xe0, 0xe0, 0xe0),
            text_muted: Color32::from_rgb(0x90, 0x90, 0x90),
            accent,
            on_accent: background,
            border: Color32::from_gray(60),
            hover: accent.gamma_multiply(0.3),
            selection: accent,
            widget: Color32::from_gray(60),
            widget_hover: Color32::from_gray(70),
            error: Color32::from_rgb(0xe0, 0x6c, 0x75),
        }
    }
}

pub fn rounding() -> egui::Rounding {
    egui::Rounding::same(10.0)
}

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default())
}

fn hex_to_color32(hex: &str) -> Option<Color32> {
    let hex = hex.trim().trim_start_matches('#');
    let digit = |i: usize| u8::from_str_radix(hex.get(i..i + 1)?, 16).ok();
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        3 | 4 => {
            let (r, g, b) = (digit(0)? * 17, digit(1)? * 17, digit(2)? * 17);
            let a = if hex.len() == 4 { digit(3)? * 17 } else { 255 };
            Some(Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        6 | 8 => {
            let a = if hex.len() == 8 { byte(6)? } else { 255 };
            Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, a))
        }
        _ => None,
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

fn luminance(c: Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

fn contrast_on(bg: Color32) -> Color32 {
    if luminance(bg) > 0.5 {
        Color32::from_rgb(0x10, 0x10, 0x10)
    } else {
        Color32::from_rgb(0xf0, 0xf0, 0xf0)
    }
}

fn raise(bg: Color32, t: f32) -> Color32 {
    let target = if luminance(bg) > 0.5 { Color32::BLACK } else { Color32::WHITE };
    mix(bg, target, t)
}

fn derived(background: Color32, text: Color32, accent: Color32) -> Palette {
    Palette {
        background,
        panel: raise(background, 0.06),
        text,
        text_muted: mix(text, background, 0.4),
        accent,
        on_accent: contrast_on(accent),
        border: mix(background, text, 0.18),
        hover: accent.gamma_multiply(0.3),
        selection: accent,
        widget: raise(background, 0.12),
        widget_hover: raise(background, 0.18),
        error: Palette::default().error,
    }
}

fn read_noctalia_palette() -> Option<Palette> {
    let raw = fs::read_to_string(home().join(KITTY_THEME_PATH)).ok()?;

    let mut map: HashMap<&str, Color32> = HashMap::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if let Some(color) = hex_to_color32(value) {
                map.insert(key, color);
            }
        }
    }

    let background = *map.get("background")?;
    let foreground = *map.get("foreground")?;
    // color5 es el slot donde noctalia deja el acento real.
    let accent = map.get("color5").or_else(|| map.get("color2")).copied().unwrap_or(foreground);
    Some(Palette {
        background,
        panel: map
            .get("selection_background")
            .or_else(|| map.get("color0"))
            .copied()
            .unwrap_or(background),
        text: foreground,
        text_muted: map.get("color8").copied().unwrap_or(foreground),
        accent,
        on_accent: background,
        hover: accent.gamma_multiply(0.3),
        selection: accent,
        error: map.get("color1").copied().unwrap_or(Palette::default().error),
        ..Palette::default()
    })
}

fn dms_files() -> Vec<PathBuf> {
    vec![home().join(DMS_COLORS_PATH)]
}

fn read_dms_palette() -> Result<Palette, String> {
    let path = home().join(DMS_COLORS_PATH);
    let raw = fs::read_to_string(&path).map_err(|_| format!("no se encontró {}", tilde(&path)))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| format!("no se pudo leer {}", tilde(&path)))?;
    let mode = json.get("mode").and_then(|m| m.as_str()).unwrap_or("dark");
    let colors = json
        .get("colors")
        .and_then(|c| c.get(mode).or_else(|| c.get("dark")))
        .ok_or_else(|| format!("{} no trae colores", tilde(&path)))?;
    let get = |key: &str| colors.get(key).and_then(|v| v.as_str()).and_then(hex_to_color32);
    let first = |keys: &[&str]| keys.iter().find_map(|k| get(k));

    let background = first(&["background", "surface"]).ok_or("faltan colores")?;
    let text = first(&["on_surface", "on_background"]).ok_or("faltan colores")?;
    let accent = get("primary").ok_or("faltan colores")?;

    let base = derived(background, text, accent);
    Ok(Palette {
        panel: first(&["surface_container", "surface_container_low"]).unwrap_or(base.panel),
        text_muted: get("on_surface_variant").unwrap_or(base.text_muted),
        on_accent: get("on_primary").unwrap_or(base.on_accent),
        border: first(&["outline_variant", "outline"]).unwrap_or(base.border),
        widget: get("surface_container_high").unwrap_or(base.widget),
        widget_hover: get("surface_container_highest").unwrap_or(base.widget_hover),
        error: get("error").unwrap_or(base.error),
        ..base
    })
}

struct GtkCss {
    defs: HashMap<String, String>,
    files: Vec<PathBuf>,
}

fn strip_css_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn read_gtk_css(path: &Path, css: &mut GtkCss, seen: &mut HashSet<PathBuf>) -> Result<(), String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(canonical) {
        return Ok(());
    }
    css.files.push(path.to_path_buf());
    let raw = fs::read_to_string(path).map_err(|_| format!("no se encontró {}", tilde(path)))?;
    let dir = path.parent().unwrap_or(Path::new("."));

    for statement in strip_css_comments(&raw).split(';') {
        let statement = statement.trim();
        if let Some(target) = statement.strip_prefix("@import") {
            let target = target.trim();
            let target = target
                .strip_prefix("url(")
                .and_then(|t| t.strip_suffix(')'))
                .unwrap_or(target)
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            let target = target.strip_prefix("file://").unwrap_or(target);
            if !target.is_empty() {
                let _ = read_gtk_css(&dir.join(target), css, seen);
            }
        } else if let Some(idx) = statement.rfind("@define-color") {
            // desde la última aparición: el trozo puede empezar con el final de una regla anterior.
            let def = statement[idx + "@define-color".len()..].trim();
            if let Some((name, value)) = def.split_once(char::is_whitespace) {
                css.defs.insert(name.trim().to_string(), value.trim().to_string());
            }
        }
    }
    Ok(())
}

fn split_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(args[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(args[start..].trim());
    out
}

fn css_channel(s: &str) -> Option<u8> {
    let v = match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<f32>().ok()? * 2.55,
        None => s.parse::<f32>().ok()?,
    };
    Some(v.round().clamp(0.0, 255.0) as u8)
}

fn resolve_gtk_color(value: &str, defs: &HashMap<String, String>, depth: u8) -> Option<Color32> {
    if depth > 16 {
        return None;
    }
    let value = value.trim();
    if let Some(name) = value.strip_prefix('@') {
        return resolve_gtk_color(defs.get(name)?, defs, depth + 1);
    }
    if value.starts_with('#') {
        return hex_to_color32(value);
    }
    let (func, args) = value.split_once('(')?;
    let args = split_args(args.strip_suffix(')')?);
    match func.trim() {
        "rgb" | "rgba" if args.len() >= 3 => {
            let alpha = args.get(3).and_then(|a| a.parse::<f32>().ok()).unwrap_or(1.0);
            Some(Color32::from_rgba_unmultiplied(
                css_channel(args[0])?,
                css_channel(args[1])?,
                css_channel(args[2])?,
                (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            ))
        }
        "alpha" if args.len() == 2 => {
            let c = resolve_gtk_color(args[0], defs, depth + 1)?;
            let a = args[1].parse::<f32>().ok()?.clamp(0.0, 1.0);
            Some(Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * a).round() as u8))
        }
        "shade" if args.len() == 2 => {
            let c = resolve_gtk_color(args[0], defs, depth + 1)?;
            let k = args[1].parse::<f32>().ok()?;
            let ch = |x: u8| (x as f32 * k).round().clamp(0.0, 255.0) as u8;
            Some(Color32::from_rgba_unmultiplied(ch(c.r()), ch(c.g()), ch(c.b()), c.a()))
        }
        "mix" if args.len() == 3 => {
            let a = resolve_gtk_color(args[0], defs, depth + 1)?;
            let b = resolve_gtk_color(args[1], defs, depth + 1)?;
            Some(mix(a, b, args[2].parse().ok()?))
        }
        _ => None,
    }
}

fn read_gtk3_palette() -> (Result<Palette, String>, Vec<PathBuf>) {
    let gtk3 = home().join(GTK3_CSS_PATH);
    let gtk4 = home().join(GTK4_CSS_PATH);
    let entry = if gtk3.exists() || !gtk4.exists() { gtk3.clone() } else { gtk4.clone() };
    let mut css = GtkCss {
        defs: HashMap::new(),
        files: Vec::new(),
    };
    if entry != gtk3 {
        css.files.push(gtk3.clone());
    }
    if read_gtk_css(&entry, &mut css, &mut HashSet::new()).is_err() {
        let msg = format!("no se encontró {} ni {}", tilde(&gtk3), tilde(&gtk4));
        return (Err(msg), vec![gtk3, gtk4]);
    }
    let get = |names: &[&str]| names.iter().find_map(|n| resolve_gtk_color(&format!("@{n}"), &css.defs, 0));

    let (Some(background), Some(text)) = (
        get(&["window_bg_color", "theme_bg_color"]),
        get(&["window_fg_color", "theme_fg_color", "view_fg_color"]),
    ) else {
        let msg = format!("{} no define colores de ventana", tilde(&entry));
        return (Err(msg), css.files);
    };
    let accent = get(&["accent_bg_color", "accent_color", "theme_selected_bg_color"])
        .unwrap_or(Palette::default().accent);

    let base = derived(background, text, accent);
    let palette = Palette {
        panel: get(&["view_bg_color", "theme_base_color", "headerbar_bg_color"]).unwrap_or(base.panel),
        on_accent: get(&["accent_fg_color", "theme_selected_fg_color"]).unwrap_or(base.on_accent),
        border: get(&["borders", "unfocused_borders"]).unwrap_or(base.border),
        widget: get(&["card_bg_color"]).unwrap_or(base.widget),
        error: get(&["error_color", "error_bg_color"]).unwrap_or(base.error),
        ..base
    };
    (Ok(palette), css.files)
}

fn tilde(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn background_opacity() -> f32 {
    (|| -> Option<f32> {
        let raw = fs::read_to_string(home().join(KITTY_CONF_PATH)).ok()?;
        raw.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != "background_opacity" {
                return None;
            }
            parts.next()?.parse().ok()
        })
    })()
    .unwrap_or(1.0)
    .clamp(0.0, 1.0)
}

static APPLIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn apply_palette(ctx: &egui::Context, palette: &Palette) {
    use std::sync::atomic::Ordering;
    if APPLIED.load(Ordering::Relaxed) && *current_palette().lock().unwrap() == *palette {
        return;
    }
    APPLIED.store(true, Ordering::Relaxed);
    let alpha = (background_opacity() * 255.0).round() as u8;

    let mut visuals = if luminance(palette.background) > 0.5 {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    let translucent_bg = Color32::from_rgba_unmultiplied(
        palette.background.r(),
        palette.background.g(),
        palette.background.b(),
        alpha,
    );
    visuals.panel_fill = translucent_bg;
    visuals.window_fill = translucent_bg;
    visuals.extreme_bg_color = translucent_bg;
    visuals.window_stroke.color = palette.border;

    visuals.override_text_color = Some(palette.text);
    visuals.hyperlink_color = palette.accent;

    visuals.selection.bg_fill = palette.selection;
    visuals.selection.stroke.color = palette.on_accent;

    visuals.widgets.noninteractive.bg_stroke.color = palette.border;
    visuals.widgets.inactive.bg_fill = palette.widget;
    visuals.widgets.inactive.weak_bg_fill = palette.widget;
    visuals.widgets.hovered.weak_bg_fill = palette.widget_hover;
    visuals.widgets.hovered.bg_fill = palette.hover;
    visuals.widgets.hovered.fg_stroke.color = palette.text;
    visuals.widgets.active.bg_fill = palette.accent;
    // este color también lo usa `RichText::strong()`: sobre un label sin fondo de acento queda invisible; usar `accent_color()`.
    visuals.widgets.active.fg_stroke.color = palette.on_accent;
    visuals.widgets.inactive.fg_stroke.color = palette.text_muted;
    visuals.widgets.noninteractive.fg_stroke.color = palette.text;

    visuals.error_fg_color = palette.error;
    visuals.warn_fg_color = palette.error;

    let rounding = self::rounding();
    visuals.window_rounding = rounding;
    visuals.menu_rounding = rounding;
    visuals.widgets.noninteractive.rounding = rounding;
    visuals.widgets.inactive.rounding = rounding;
    visuals.widgets.hovered.rounding = rounding;
    visuals.widgets.active.rounding = rounding;
    visuals.widgets.open.rounding = rounding;

    ctx.set_visuals(visuals);
    *current_palette().lock().unwrap() = *palette;
    ctx.request_repaint();
}

fn current_palette() -> &'static Mutex<Palette> {
    static PALETTE: OnceLock<Mutex<Palette>> = OnceLock::new();
    PALETTE.get_or_init(|| Mutex::new(Palette::default()))
}

struct Watcher {
    source: ThemeSource,
    files: Vec<(PathBuf, Option<SystemTime>)>,
    loaded: bool,
    retry: bool,
    status: Option<String>,
    next_check: Instant,
}

fn watcher() -> &'static Mutex<Watcher> {
    static WATCHER: OnceLock<Mutex<Watcher>> = OnceLock::new();
    WATCHER.get_or_init(|| {
        Mutex::new(Watcher {
            source: ThemeSource::Default,
            files: Vec::new(),
            loaded: false,
            retry: false,
            status: None,
            next_check: Instant::now(),
        })
    })
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn reload(ctx: &egui::Context, w: &mut Watcher) {
    let (result, files) = match w.source {
        ThemeSource::Default => (
            Ok(read_noctalia_palette().unwrap_or_default()),
            vec![home().join(KITTY_THEME_PATH)],
        ),
        ThemeSource::Dms => (read_dms_palette(), dms_files()),
        ThemeSource::Gtk3 => read_gtk3_palette(),
    };
    w.files = files
        .into_iter()
        .map(|path| {
            let seen = mtime(&path);
            (path, seen)
        })
        .collect();

    match result {
        Ok(palette) => {
            apply_palette(ctx, &palette);
            w.loaded = true;
            w.retry = false;
            w.status = None;
        }
        Err(reason) => {
            let fallback = if w.loaded {
                "se mantiene el último tema leído"
            } else {
                apply_palette(ctx, &read_noctalia_palette().unwrap_or_default());
                "usando el tema por defecto"
            };
            w.retry = w.files.iter().any(|(_, seen)| seen.is_some());
            w.status = Some(format!("{} no disponible ({reason}); {fallback}.", w.source.label()));
        }
    }
}

pub fn init(ctx: &egui::Context, source: ThemeSource) {
    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
    });
    set_source(ctx, source);

    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("theme-watch".into()).spawn(move || loop {
        let wait = watcher().lock().unwrap().next_check.saturating_duration_since(Instant::now());
        std::thread::sleep(wait.max(Duration::from_millis(100)));
        check(&ctx);
    });
}

pub fn set_source(ctx: &egui::Context, source: ThemeSource) {
    let mut w = watcher().lock().unwrap();
    w.source = source;
    w.loaded = false;
    reload(ctx, &mut w);
    w.next_check = Instant::now() + CHECK_INTERVAL;
}

pub fn source() -> ThemeSource {
    watcher().lock().unwrap().source
}

pub fn status() -> Option<String> {
    watcher().lock().unwrap().status.clone()
}

fn check(ctx: &egui::Context) {
    let mut w = watcher().lock().unwrap();
    let now = Instant::now();
    if now < w.next_check {
        return;
    }
    w.next_check = now + CHECK_INTERVAL;

    let mut changed = w.retry;
    let mut settling = false;
    for (path, seen) in &w.files {
        let current = mtime(path);
        if current != *seen {
            changed = true;
            settling |= current.and_then(|m| m.elapsed().ok()).is_some_and(|age| age < SETTLE_TIME);
        }
    }
    if settling {
        w.next_check = now + SETTLE_TIME;
    } else if changed {
        reload(ctx, &mut w);
    }
}

pub fn card_fill() -> Color32 {
    current_palette().lock().unwrap().panel
}

pub fn accent_color() -> Color32 {
    current_palette().lock().unwrap().accent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn resuelve_colores_gtk() {
        let d = defs(&[
            ("a", "#112233"),
            ("b", "@a"),
            ("c", "rgba(255, 0, 0, 0.5)"),
            ("d", "#fff"),
            ("e", "alpha(@a, 0.5)"),
            ("loop1", "@loop2"),
            ("loop2", "@loop1"),
        ]);
        assert_eq!(resolve_gtk_color("@b", &d, 0), Some(Color32::from_rgb(0x11, 0x22, 0x33)));
        assert_eq!(resolve_gtk_color("@c", &d, 0), Some(Color32::from_rgba_unmultiplied(255, 0, 0, 128)));
        assert_eq!(resolve_gtk_color("@d", &d, 0), Some(Color32::WHITE));
        assert_eq!(resolve_gtk_color("rgb(10%, 0, 255)", &d, 0), Some(Color32::from_rgb(26, 0, 255)));
        assert_eq!(resolve_gtk_color("@e", &d, 0).map(|c| c.a()), Some(128));
        assert_eq!(resolve_gtk_color("@loop1", &d, 0), None);
        assert_eq!(resolve_gtk_color("@falta", &d, 0), None);
    }

    #[test]
    fn lee_gtk_css_con_imports() {
        let dir = std::env::temp_dir().join(format!("psst-theme-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("gtk.css"),
            "/* x */ @import url(\"colors.css\");\n@define-color window_fg_color #eeeeee;\n.foo { color: red; }",
        )
        .unwrap();
        fs::write(
            dir.join("colors.css"),
            "@define-color window_bg_color #202020;\n@define-color window_fg_color #000000; /* pisado */",
        )
        .unwrap();
        let mut css = GtkCss {
            defs: HashMap::new(),
            files: Vec::new(),
        };
        read_gtk_css(&dir.join("gtk.css"), &mut css, &mut HashSet::new()).unwrap();
        assert_eq!(css.files.len(), 2);
        assert_eq!(css.defs.get("window_bg_color").map(String::as_str), Some("#202020"));
        assert_eq!(css.defs.get("window_fg_color").map(String::as_str), Some("#eeeeee"));
        let _ = fs::remove_dir_all(&dir);
    }
}
