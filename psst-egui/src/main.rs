mod browse_view;
mod fonts;
mod image_cache;
mod search_view;
mod settings_ui;
mod theme;

use std::collections::{HashMap, HashSet};

use eframe::egui;
use librespot_core::FileId;
use psst_core::player::queue::QueueBehavior;
use psst_backend::{
    actions,
    cover,
    library::{self, PlaylistRef, TrackRef},
    login,
    lyrics::{self, LyricLine},
    mpris,
    playback::{Playback, QueueTrack},
    resume, romanize, search, settings, sysvolume::SystemVolume, translate,
};

#[derive(PartialEq)]
struct OrderKey {
    tracks_ptr: usize,
    tracks_len: usize,
    query: String,
    sort_mode: SortMode,
    meta_rev: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Original,
    Name,
    Artist,
    Album,
    Added,
    Duration,
}

fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn fmt_total_duration(d: std::time::Duration) -> String {
    let mins = d.as_secs() / 60;
    if mins >= 60 {
        format!("{} h {} min", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

/// Se aplica después de dibujar: los closures de egui ya tienen `self` prestado.
enum RowAction {
    ToggleLike(String),
    AddToPlaylist {
        playlist_uri: String,
        playlist_name: String,
        track_uri: String,
    },
}

enum ActionKind {
    Like { uri: String, liked: bool },
    AddToPlaylist {
        playlist_uri: String,
        playlist_name: String,
        track_uri: String,
    },
}

struct ActionResult {
    kind: ActionKind,
    result: Result<(), String>,
}

fn track_context_menu(
    ui: &mut egui::Ui,
    track_uri: &str,
    liked: bool,
    own_playlists: &[(String, String)],
    out: &mut Vec<RowAction>,
) {
    let label = if liked { "♥ Quitar de Me gusta" } else { "♥ Agregar a Me gusta" };
    if ui.button(label).clicked() {
        out.push(RowAction::ToggleLike(track_uri.to_string()));
        ui.close_menu();
    }
    ui.menu_button("Agregar a playlist", |ui| {
        if own_playlists.is_empty() {
            ui.label(egui::RichText::new("(no tenés playlists propias)").weak());
        }
        for (uri, name) in own_playlists {
            if ui.button(name).clicked() {
                out.push(RowAction::AddToPlaylist {
                    playlist_uri: uri.clone(),
                    playlist_name: name.clone(),
                    track_uri: track_uri.to_string(),
                });
                ui.close_menu();
            }
        }
    });
}

struct App {
    rt: tokio::runtime::Runtime,
    session: librespot_core::session::Session,
    playback: Playback,
    playlists: Vec<PlaylistRef>,
    selected_playlist: Option<usize>,
    tracks: Vec<TrackRef>,
    loaded_playlist_uri: Option<String>,
    tracks_label: String,
    search_query: String,
    sort_mode: SortMode,
    playlist_textures: HashMap<String, egui::TextureHandle>,
    cover_texture: Option<egui::TextureHandle>,
    cover_shown: Option<FileId>,
    /// no se guarda en `PlaylistRef.owner`: el username crudo hace falta intacto como clave de cache.
    owner_name: Option<Result<String, search::NameError>>,
    lyrics_visible: bool,
    lyrics: Vec<LyricLine>,
    lyrics_track_uri: Option<String>,
    lyrics_status: Option<String>,
    /// fuerza el primer chequeo: sin nada sonando, comparar `None` con `None` nunca dispararía.
    lyrics_needs_refresh: bool,
    translations_visible: bool,
    translated_lines: Vec<String>,
    translation_track_uri: Option<String>,
    translation_status: Option<String>,
    translation_needs_refresh: bool,
    translate_lang: String,
    system_volume: SystemVolume,
    pronunciation_visible: bool,
    pronunciation_lines: Vec<String>,
    settings_ui: settings_ui::SettingsUi,
    last_resume_save: std::time::Instant,
    last_resume_saved: Option<resume::ResumeState>,
    order_cache: Option<(OrderKey, Vec<usize>)>,
    liked: HashSet<String>,
    /// fuerza recargar "Me gusta"; no se toca `loaded_playlist_uri` porque resume lo necesita.
    collection_stale: bool,
    action_tx: UiSender<ActionResult>,
    action_rx: std::sync::mpsc::Receiver<ActionResult>,
    search: Option<search_view::SearchState>,
    search_tx: UiSender<search_view::SearchMsg>,
    search_rx: std::sync::mpsc::Receiver<search_view::SearchMsg>,
    search_generation: u64,
    nav: Vec<browse_view::Screen>,
    browse_tx: UiSender<browse_view::BrowseMsg>,
    browse_rx: std::sync::mpsc::Receiver<browse_view::BrowseMsg>,
    browse_next_id: u64,
    toast: Option<(String, std::time::Instant)>,
    ui_scale: f32,
    now_playing_open: bool,
    min_size_sent: Option<egui::Vec2>,
    queue_playlist_uri: Option<String>,
    /// el stream de PipeWire recién existe cuando empieza a sonar algo.
    pending_volume: Option<f64>,
    window_size: Option<[f32; 2]>,
    window_pos: Option<[f32; 2]>,
    window_maximized: bool,
    saved_settings: settings::Settings,
    last_settings_check: std::time::Instant,
    reload_tx: UiSender<(String, Vec<TrackRef>)>,
    reload_rx: std::sync::mpsc::Receiver<(String, Vec<TrackRef>)>,
    table_top_row: Option<usize>,
    scroll_to_row: Option<usize>,
    cover_large_texture: Option<egui::TextureHandle>,
    cover_large_shown: Option<FileId>,
    thumbs: HashMap<FileId, Option<egui::TextureHandle>>,
    thumb_tx: std::sync::mpsc::Sender<(FileId, Option<(Vec<u8>, u32, u32)>)>,
    thumb_rx: std::sync::mpsc::Receiver<(FileId, Option<(Vec<u8>, u32, u32)>)>,
    playlists_rx: std::sync::mpsc::Receiver<Vec<PlaylistRef>>,
    last_meta_rev: u64,
}

impl App {
    fn load_tracks(&mut self, playlist_idx: usize) {
        self.nav.clear();
        let uri = self.playlists[playlist_idx].uri.clone();
        let stale = uri == "collection" && self.collection_stale;
        if self.loaded_playlist_uri.as_deref() == Some(uri.as_str()) && !stale {
            return;
        }
        self.collection_stale = false;
        self.tracks = self.rt.block_on(library::tracks(&self.session, &uri));
        self.loaded_playlist_uri = Some(uri);
        self.tracks_label = self.playlists[playlist_idx].name.clone();
        self.search_query.clear();
        self.search = None;

        let playlist = self.playlists[playlist_idx].clone();
        self.resolve_owner_name_for(&playlist);
    }

    fn resolve_owner_name_for(&mut self, playlist: &PlaylistRef) {
        let raw_owner = playlist.owner.clone();
        let is_self = raw_owner == self.session.username();
        self.owner_name = Some(search::display_name_for(&self.session, self.rt.handle(), &raw_owner, is_self));
    }

    fn playlist_header(&mut self, ui: &mut egui::Ui, p: &PlaylistRef) {
        let width = ui.available_width();
        let (cover_size, title_size, meta_size) = if width > 820.0 {
            (180.0, 34.0, 16.0)
        } else if width > 620.0 {
            (128.0, 26.0, 15.0)
        } else {
            (84.0, 20.0, 14.0)
        };

        ui.horizontal(|ui| {
            if let Some(texture) = self.playlist_textures.get(&p.uri) {
                ui.add(egui::Image::new(texture).max_size(egui::vec2(cover_size, cover_size)).rounding(theme::rounding()));
            } else if let Some(url) = &p.cover_url {
                ui.add(
                    image_cache::image(url)
                        .fit_to_exact_size(egui::vec2(cover_size, cover_size))
                        .rounding(theme::rounding()),
                );
            } else {
                ui.add_space(cover_size);
            }
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                // sin `.strong()`: el tema lo pinta con el color de fondo y queda ilegible acá.
                ui.label(egui::RichText::new(&p.name).size(title_size));
                if !p.description.is_empty() {
                    ui.label(egui::RichText::new(&p.description).weak().size(meta_size));
                }
                let total: std::time::Duration = self.tracks.iter().map(|t| t.duration).sum();
                let track_count = self.tracks.len();
                let is_self = p.owner == self.session.username();
                let (owner, tooltip) = match &self.owner_name {
                    Some(Ok(name)) => (name.clone(), format!("ID: {}", p.owner)),
                    other => {
                        let fallback = if is_self { "Tú" } else { "Usuario de Spotify" };
                        let reason = match other {
                            Some(Err(e)) => format!("\nNo se pudo obtener el nombre: {e}"),
                            _ => String::new(),
                        };
                        (fallback.to_string(), format!("ID: {}{reason}", p.owner))
                    }
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {} canciones · {}",
                        owner,
                        track_count,
                        fmt_total_duration(total)
                    ))
                    .weak()
                    .size(meta_size),
                )
                .on_hover_text(tooltip);
            });
        });
        ui.add_space(6.0);
        ui.separator();
    }

    /// Solo las propias: en las de otros la Web API devuelve 403.
    fn own_playlists(&self) -> Vec<(String, String)> {
        let me = self.session.username();
        self.playlists
            .iter()
            .filter(|p| p.uri.starts_with("spotify:playlist:") && p.owner == me)
            .map(|p| (p.uri.clone(), p.name.clone()))
            .collect()
    }

    fn apply_row_action(&mut self, action: RowAction) {
        match action {
            RowAction::ToggleLike(uri) => {
                let liked = !self.liked.contains(&uri);
                if liked {
                    self.liked.insert(uri.clone());
                } else {
                    self.liked.remove(&uri);
                }
                self.spawn_action(ActionKind::Like { uri, liked });
            }
            RowAction::AddToPlaylist { playlist_uri, playlist_name, track_uri } => {
                self.spawn_action(ActionKind::AddToPlaylist { playlist_uri, playlist_name, track_uri });
            }
        }
    }

    fn spawn_action(&self, kind: ActionKind) {
        let tx = self.action_tx.clone();
        let _ = std::thread::Builder::new().name("webapi-action".into()).spawn(move || {
            let result = match search::webapi_token_if_available() {
                None => Err("sin token de Web API (hacé una búsqueda una vez para configurarlo)".to_string()),
                Some(token) => match &kind {
                    ActionKind::Like { uri, liked } => actions::set_liked(&token, uri, *liked),
                    ActionKind::AddToPlaylist { playlist_uri, track_uri, .. } => {
                        actions::add_to_playlist(&token, playlist_uri, track_uri)
                    }
                },
            };
            let _ = tx.send(ActionResult { kind, result });
        });
    }

    fn sync_backfilled_artists(&mut self) {
        let st = self.playback.status.lock().unwrap();
        if st.meta_rev == self.last_meta_rev {
            return;
        }
        self.last_meta_rev = st.meta_rev;
        let (Some(uri), Some(artist)) = (st.track_uri.clone(), st.artist.clone()) else {
            return;
        };
        let artists = st.artists.clone();
        drop(st);
        for t in self.tracks.iter_mut().filter(|t| t.uri == uri) {
            t.artist = artist.clone();
            t.artists = artists.clone();
        }
    }

    fn volume(&self) -> f64 {
        self.system_volume
            .get()
            .or(self.pending_volume)
            .unwrap_or_else(|| self.playback.status.lock().unwrap().volume)
    }

    fn set_volume(&mut self, volume: f64) {
        let volume = volume.clamp(0.0, 1.0);
        if self.system_volume.set(volume) {
            self.pending_volume = None;
        } else {
            self.playback.set_volume(volume);
            self.pending_volume = Some(volume);
        }
    }

    /// Sin repintado periódico en pausa: la UI se despierta por entrada o por avisos de otros hilos (`UiSender`, player, volumen, tema).
    fn schedule_repaint(&self, ctx: &egui::Context) {
        let (minimized, focused) = ctx.input(|i| {
            let vp = i.viewport();
            (vp.minimized.unwrap_or(false), vp.focused.unwrap_or(true))
        });
        if minimized || !self.playback.status.lock().unwrap().playing {
            return;
        }
        // sin foco (Hyprland no informa otro workspace como minimizada) basta 1 s; con letras se mantiene el ritmo del karaoke
        let ms = if focused || self.lyrics_visible { 250 } else { 1000 };
        ctx.request_repaint_after(std::time::Duration::from_millis(ms));
    }

    /// `consume_key` evita que un botón con foco reciba también el Espacio.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        let search_id = search_box_id();

        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
            ctx.memory_mut(|m| m.request_focus(search_id));
        }
        if ctx.memory(|m| m.has_focus(search_id)) && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            ctx.memory_mut(|m| m.surrender_focus(search_id));
        }
        // `Equals` es el "+" sin Shift del teclado US.
        let ctrl = |key| ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, key));
        if ctrl(Key::Plus) || ctrl(Key::Equals) {
            self.set_ui_scale(ctx, self.ui_scale + settings::UI_SCALE_STEP);
        }
        if ctrl(Key::Minus) {
            self.set_ui_scale(ctx, self.ui_scale - settings::UI_SCALE_STEP);
        }
        if ctrl(Key::Num0) {
            self.set_ui_scale(ctx, settings::DEFAULT_UI_SCALE);
        }
        if ctrl(Key::R) {
            self.toggle_now_playing();
        }
        if ctx.wants_keyboard_input() {
            return;
        }

        if !self.nav.is_empty() && ctx.input_mut(|i| i.consume_key(Modifiers::ALT, Key::ArrowLeft)) {
            self.go_back();
        }

        if let Some(search) = &mut self.search {
            let keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
            for (key, chip) in keys.into_iter().zip(search_view::Chip::ALL) {
                if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, key)) {
                    search.chip = chip;
                }
            }
        }

        let pressed = |key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key));
        if pressed(Key::Space) {
            self.playback.pause_or_resume();
        }
        if pressed(Key::ArrowLeft) {
            self.playback.seek_relative(-5.0);
        }
        if pressed(Key::ArrowRight) {
            self.playback.seek_relative(5.0);
        }
        if pressed(Key::ArrowUp) {
            self.set_volume(self.volume() + 0.05);
        }
        if pressed(Key::ArrowDown) {
            self.set_volume(self.volume() - 0.05);
        }
        if pressed(Key::N) {
            self.playback.next();
        }
        if pressed(Key::P) {
            self.playback.previous();
        }
    }

    fn drain_actions(&mut self) {
        while let Ok(res) = self.action_rx.try_recv() {
            let msg = match (res.kind, res.result) {
                (ActionKind::Like { liked, .. }, Ok(())) => {
                    self.collection_stale = true;
                    if liked { "Agregada a Me gusta" } else { "Quitada de Me gusta" }.to_string()
                }
                (ActionKind::Like { uri, liked }, Err(e)) => {
                    if liked {
                        self.liked.remove(&uri);
                    } else {
                        self.liked.insert(uri);
                    }
                    format!("No se pudo actualizar Me gusta: {e}")
                }
                (ActionKind::AddToPlaylist { playlist_uri, playlist_name, .. }, Ok(())) => {
                    if self.loaded_playlist_uri.as_deref() == Some(playlist_uri.as_str()) {
                        if let Some(idx) = self.playlists.iter().position(|p| p.uri == playlist_uri) {
                            self.loaded_playlist_uri = None;
                            self.load_tracks(idx);
                        }
                    }
                    format!("Agregada a {playlist_name}")
                }
                (ActionKind::AddToPlaylist { playlist_name, .. }, Err(e)) => {
                    format!("No se pudo agregar a {playlist_name}: {e}")
                }
            };
            let msg: String = msg.chars().take(160).collect();
            self.toast = Some((msg, std::time::Instant::now()));
        }
    }

    fn show_toast(&mut self, ctx: &egui::Context) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > std::time::Duration::from_secs(5))
        {
            self.toast = None;
        }
        let Some((msg, shown_at)) = &self.toast else {
            return;
        };
        // sin repintado periódico, hay que pedir uno para cuando se vence
        ctx.request_repaint_after(std::time::Duration::from_secs(5).saturating_sub(shown_at.elapsed()));
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -96.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(theme::card_fill())
                    .rounding(theme::rounding())
                    .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                    .show(ui, |ui| {
                        ui.set_max_width(420.0);
                        ui.label(msg.as_str());
                    });
            });
    }

    fn update_cover(&mut self, ctx: &egui::Context) {
        let cover = self.playback.status.lock().unwrap().cover;
        if cover == self.cover_shown {
            return;
        }
        self.cover_shown = cover;
        self.cover_texture = cover.and_then(|file_id| {
            self.rt
                .block_on(cover::fetch_cover_rgba(&self.session, file_id))
                .ok()
                .map(|(rgba, w, h)| {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    ctx.load_texture("cover", image, egui::TextureOptions::default())
                })
        });
    }

    fn update_lyrics(&mut self) {
        if !self.lyrics_visible {
            return;
        }
        let track_uri = self.playback.status.lock().unwrap().track_uri.clone();
        if track_uri == self.lyrics_track_uri && !self.lyrics_needs_refresh {
            return;
        }
        self.lyrics_needs_refresh = false;
        self.lyrics_track_uri = track_uri.clone();
        self.lyrics.clear();
        self.pronunciation_lines.clear();
        let Some(uri) = track_uri else {
            self.lyrics_status = Some("nada sonando todavía".to_string());
            return;
        };
        match self.rt.block_on(lyrics::fetch_lyrics(&self.session, &uri)) {
            Ok(lines) if lines.is_empty() => {
                self.lyrics_status = Some("sin letras para esta canción".to_string());
            }
            Ok(lines) => {
                let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
                self.pronunciation_lines = romanize::romanize_lines(&texts);
                self.lyrics = lines;
                self.lyrics_status = None;
            }
            Err(e) => self.lyrics_status = Some(e),
        }
    }

    fn update_translation(&mut self) {
        if !self.translations_visible {
            return;
        }
        if self.lyrics_track_uri == self.translation_track_uri && !self.translation_needs_refresh {
            return;
        }
        self.translation_needs_refresh = false;
        self.translation_track_uri = self.lyrics_track_uri.clone();
        self.translated_lines.clear();
        self.translation_status = None;

        let Some(uri) = self.lyrics_track_uri.clone() else {
            return;
        };
        if self.lyrics.is_empty() {
            return;
        }
        let lines: Vec<String> = self.lyrics.iter().map(|l| l.text.clone()).collect();
        match translate::translated_lyrics(&uri, &self.translate_lang, &lines) {
            Ok(translated) => self.translated_lines = translated,
            Err(e) => self.translation_status = Some(e),
        }
    }

    fn current_settings(&self) -> settings::Settings {
        settings::Settings {
            translate_lang: self.translate_lang.clone(),
            translations_visible: self.translations_visible,
            pronunciation_visible: self.pronunciation_visible,
            ui_scale: self.ui_scale,
            now_playing_panel: self.now_playing_open,
            volume: Some(self.pending_volume.unwrap_or_else(|| self.volume())),
            window_size: self.window_size,
            window_pos: self.window_pos,
            window_maximized: self.window_maximized,
            last_playlist_uri: self.selected_playlist.and_then(|i| self.playlists.get(i)).map(|p| p.uri.clone()),
            theme_source: theme::source().as_setting().to_string(),
        }
    }

    fn save_settings(&mut self) {
        let current = self.current_settings();
        settings::save(&current);
        self.saved_settings = current;
    }

    fn persist_ui_state(&mut self, ctx: &egui::Context) {
        // puntos de egui × zoom = unidades lógicas de la ventana (las de `ViewportBuilder`).
        let zoom = ctx.zoom_factor();
        ctx.input(|i| {
            let vp = i.viewport();
            self.window_maximized = vp.maximized.unwrap_or(false);
            if !self.window_maximized && !vp.fullscreen.unwrap_or(false) {
                if let Some(rect) = vp.inner_rect {
                    self.window_size = Some((rect.size() * zoom).into());
                }
                if let Some(rect) = vp.outer_rect {
                    self.window_pos = Some((rect.min.to_vec2() * zoom).into());
                }
            }
        });

        if let Some(volume) = self.pending_volume {
            // el interno vuelve a 1: si no, quedarían los dos volúmenes aplicados.
            if self.system_volume.get().is_some() && self.system_volume.set(volume) {
                self.playback.set_volume(1.0);
                self.pending_volume = None;
            }
        }

        let since = self.last_settings_check.elapsed();
        let interval = std::time::Duration::from_secs(2);
        if self.current_settings() == self.saved_settings {
            return;
        }
        if since < interval {
            // hay algo por guardar: un repintado para cuando toque
            ctx.request_repaint_after(interval - since);
            return;
        }
        self.last_settings_check = std::time::Instant::now();
        self.save_settings();
    }

    fn update_min_window_size(&mut self, ctx: &egui::Context) {
        let extra = if self.now_playing_open { NOW_PLAYING_MIN_WIDTH } else { 0.0 };
        let min = (MIN_LAYOUT + egui::vec2(extra, 0.0)) * self.ui_scale;
        if self.min_size_sent != Some(min) {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min));
            self.min_size_sent = Some(min);
        }
    }

    /// En mosaico (Hyprland) el compositor puede ignorar el tamaño mínimo: el panel se oculta solo.
    fn now_playing_fits(ctx: &egui::Context) -> bool {
        ctx.screen_rect().width() >= MIN_LAYOUT.x + NOW_PLAYING_MIN_WIDTH
    }

    fn toggle_lyrics(&mut self) {
        self.lyrics_visible = !self.lyrics_visible;
        if self.lyrics_visible {
            self.lyrics_needs_refresh = true;
        }
    }

    fn toggle_now_playing(&mut self) {
        self.now_playing_open = !self.now_playing_open;
        self.save_settings();
    }

    fn update_cover_large(&mut self, ctx: &egui::Context) {
        if !self.now_playing_open {
            return;
        }
        let cover = self.playback.status.lock().unwrap().cover_large;
        if cover == self.cover_large_shown {
            return;
        }
        self.cover_large_shown = cover;
        self.cover_large_texture = cover.and_then(|file_id| {
            self.rt
                .block_on(cover::fetch_cover_rgba(&self.session, file_id))
                .ok()
                .map(|(rgba, w, h)| {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    ctx.load_texture("cover_large", image, egui::TextureOptions::LINEAR)
                })
        });
    }

    fn now_playing_panel(&mut self, ctx: &egui::Context) {
        if !self.now_playing_open || !Self::now_playing_fits(ctx) {
            return;
        }
        self.drain_thumbnails(ctx);
        let frame = egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(14.0, 10.0));
        egui::SidePanel::right("now_playing_panel")
            .resizable(true)
            .default_width(320.0)
            .width_range(260.0..=560.0)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // nada de `.strong()` en este panel: queda invisible con este tema.
                    let text = ui.visuals().text_color();
                    ui.label(egui::RichText::new("Reproduciendo").size(16.0).color(text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(egui::RichText::new("×").size(18.0)).frame(false))
                            .on_hover_text("Cerrar (Ctrl+R)")
                            .clicked()
                        {
                            self.toggle_now_playing();
                        }
                    });
                });
                ui.add_space(8.0);
                self.now_playing_content(ui);
            });
    }

    fn now_playing_content(&mut self, ui: &mut egui::Ui) {
        let st = self.playback.status.lock().unwrap().clone();
        let Some(title) = st.title.clone() else {
            ui.add_space(ui.available_height() / 3.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("♪").size(40.0).weak());
                ui.label(egui::RichText::new("Nada sonando todavía").weak());
            });
            return;
        };

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let side = ui.available_width();
            if let Some(texture) = self.cover_large_texture.as_ref().or(self.cover_texture.as_ref()) {
                egui::Frame::none()
                    .rounding(theme::rounding())
                    .shadow(egui::Shadow {
                        offset: egui::vec2(0.0, 6.0),
                        blur: 18.0,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha(90),
                    })
                    .show(ui, |ui| {
                        ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(egui::vec2(side, side))
                                .rounding(theme::rounding()),
                        );
                    });
            } else {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                ui.painter().rect_filled(rect, theme::rounding(), ui.visuals().faint_bg_color);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "♪",
                    egui::FontId::proportional(side / 4.0),
                    ui.visuals().weak_text_color(),
                );
            }

            ui.add_space(14.0);
            let text = ui.visuals().text_color();
            ui.label(egui::RichText::new(title).size(20.0).color(text));
            if !st.artists.is_empty() {
                ui.add_space(2.0);
                if let Some(target) =
                    browse_view::artist_links(ui, &st.artists, &st.artist_uris, |s| egui::RichText::new(s).size(14.0).color(text))
                {
                    self.open_artist(target);
                }
            }
            let album_line = match (&st.album, st.year) {
                (Some(album), Some(year)) => format!("{album} · {year}"),
                (Some(album), None) => album.clone(),
                (None, Some(year)) => year.to_string(),
                (None, None) => String::new(),
            };
            if !album_line.is_empty() {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(album_line).weak());
            }

            ui.add_space(18.0);
            let upcoming = self.playback.upcoming();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("A continuación").size(15.0).color(text));
                if !upcoming.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{}", upcoming.len())).weak().small());
                    });
                }
            });
            ui.add_space(6.0);
            if upcoming.is_empty() {
                ui.label(egui::RichText::new("No hay más canciones en la cola.").weak());
            }
            for (count, track) in &upcoming {
                match self.queue_row(ui, track) {
                    QueueRowClick::Artist(target) => self.open_artist(target),
                    QueueRowClick::Play => self.playback.skip_ahead(*count),
                    QueueRowClick::None => {}
                }
            }
        });
    }

    fn queue_row(&mut self, ui: &mut egui::Ui, track: &QueueTrack) -> QueueRowClick {
        const ROW_H: f32 = 52.0;
        const THUMB: f32 = 40.0;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
        if !ui.is_rect_visible(rect) {
            return QueueRowClick::None;
        }
        let hovered = response.hovered();
        let visuals = ui.visuals().clone();
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(rect, theme::rounding() * 0.6, visuals.widgets.hovered.weak_bg_fill);
        }

        let thumb_rect = egui::Rect::from_min_size(
            rect.left_center() + egui::vec2(6.0, -THUMB / 2.0),
            egui::vec2(THUMB, THUMB),
        );
        let thumb_rounding = theme::rounding() * 0.4;
        match track.cover.and_then(|id| self.thumbnail(ui.ctx(), id)) {
            Some(texture) => {
                egui::Image::new(&texture).rounding(thumb_rounding).paint_at(ui, thumb_rect);
            }
            None => {
                ui.painter().rect_filled(thumb_rect, thumb_rounding, visuals.faint_bg_color);
            }
        }
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(thumb_rect, thumb_rounding, egui::Color32::from_black_alpha(140));
            painter.text(
                thumb_rect.center(),
                egui::Align2::CENTER_CENTER,
                "▶",
                egui::FontId::proportional(16.0),
                egui::Color32::WHITE,
            );
        }

        let right = rect.right() - 8.0;
        let dur = (track.duration > std::time::Duration::ZERO).then(|| fmt_duration(track.duration));
        let dur_galley = dur.map(|d| {
            egui::WidgetText::from(egui::RichText::new(d).small())
                .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Small)
        });
        let text_left = thumb_rect.right() + 10.0;
        let text_right = right - dur_galley.as_ref().map_or(0.0, |g| g.size().x + 10.0);
        let text_width = (text_right - text_left).max(10.0);

        let title_color = if hovered { theme::accent_color() } else { visuals.text_color() };
        let title = egui::WidgetText::from(egui::RichText::new(&track.title))
            .into_galley(ui, Some(egui::TextWrapMode::Truncate), text_width, egui::TextStyle::Body);
        let artist_h = ui.text_style_height(&egui::TextStyle::Small);
        let block_h = title.size().y + 2.0 + artist_h;
        let top = rect.center().y - block_h / 2.0;
        let painter = ui.painter();
        painter.galley(egui::pos2(text_left, top), title.clone(), title_color);
        if let Some(g) = dur_galley {
            painter.galley(
                egui::pos2(right - g.size().x, rect.center().y - g.size().y / 2.0),
                g,
                visuals.weak_text_color(),
            );
        }

        let artists_rect = egui::Rect::from_min_size(
            egui::pos2(text_left, top + title.size().y + 2.0),
            egui::vec2(text_width, artist_h),
        );
        let weak = visuals.weak_text_color();
        let artist = browse_view::artist_links_at(ui, artists_rect, &track.artists, &track.artist_uris, |s| {
            egui::RichText::new(s).small().color(weak)
        });
        let clicked = response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
        match artist {
            Some(target) => QueueRowClick::Artist(target),
            None if clicked => QueueRowClick::Play,
            None => QueueRowClick::None,
        }
    }

    fn thumbnail(&mut self, ctx: &egui::Context, file_id: FileId) -> Option<egui::TextureHandle> {
        if let Some(entry) = self.thumbs.get(&file_id) {
            return entry.clone();
        }
        self.thumbs.insert(file_id, None);
        let session = self.session.clone();
        let tx = self.thumb_tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = cover::fetch_cover_rgba(&session, file_id).await.ok();
            let _ = tx.send((file_id, result));
            ctx.request_repaint();
        });
        None
    }

    fn drain_thumbnails(&mut self, ctx: &egui::Context) {
        while let Ok((file_id, result)) = self.thumb_rx.try_recv() {
            let texture = result.map(|(rgba, w, h)| {
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                ctx.load_texture(format!("thumb-{file_id:?}"), image, egui::TextureOptions::LINEAR)
            });
            self.thumbs.insert(file_id, texture);
        }
        if self.thumbs.len() > 300 {
            self.thumbs.clear();
        }
    }

    fn set_ui_scale(&mut self, ctx: &egui::Context, scale: f32) {
        let scale = settings::clamp_ui_scale(scale);
        if (scale - self.ui_scale).abs() < f32::EPSILON {
            return;
        }
        self.ui_scale = scale;
        ctx.set_zoom_factor(scale);
        self.save_settings();
    }

    fn maybe_save_resume_state(&mut self, ctx: &egui::Context) {
        let Some(playlist_uri) = self.queue_playlist_uri.clone() else {
            return;
        };
        let st = self.playback.status.lock().unwrap().clone();
        let Some(track_uri) = st.track_uri else {
            return;
        };
        let state = resume::ResumeState {
            playlist_uri,
            track_uri,
            position_ms: st.position.as_millis() as u64,
            queue_behavior: resume::queue_behavior_to_str(st.queue_behavior).to_string(),
        };
        if self.last_resume_saved.as_ref() == Some(&state) {
            return;
        }
        let since = self.last_resume_save.elapsed();
        let interval = std::time::Duration::from_secs(5);
        if since < interval {
            // cambió (ej. se pausó): repintado para guardarlo cuando toque
            ctx.request_repaint_after(interval - since);
            return;
        }
        self.last_resume_save = std::time::Instant::now();
        resume::save(&state);
        self.last_resume_saved = Some(state);
    }

    fn load_playlist_covers(&mut self, ctx: &egui::Context) {
        self.playlist_textures.retain(|uri, _| self.playlists.iter().any(|p| &p.uri == uri));
        for p in &self.playlists {
            let Some(key) = cover::playlist_cover_key(p) else {
                self.playlist_textures.remove(&p.uri);
                continue;
            };
            let name = format!("playlist-{key}");
            if self.playlist_textures.get(&p.uri).is_some_and(|t| t.name() == name) {
                continue;
            }
            if let Some((rgba, w, h)) = cover::cached_playlist_cover(p) {
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                let texture = ctx.load_texture(name, image, egui::TextureOptions::default());
                self.playlist_textures.insert(p.uri.clone(), texture);
            }
        }
    }

    fn drain_playlists(&mut self, ctx: &egui::Context) {
        let Ok(playlists) = self.playlists_rx.try_recv() else { return };
        let selected_uri = self.selected_playlist.and_then(|i| self.playlists.get(i)).map(|p| p.uri.clone());
        if let Some(loaded) = &self.loaded_playlist_uri {
            let old_rev = self.playlists.iter().find(|p| &p.uri == loaded).map(|p| p.revision.as_str());
            let new_rev = playlists.iter().find(|p| &p.uri == loaded).map(|p| p.revision.as_str());
            if let (Some(old_rev), Some(new_rev)) = (old_rev, new_rev) {
                if !new_rev.is_empty() && old_rev != new_rev {
                    let (session, tx, uri) = (self.session.clone(), self.reload_tx.clone(), loaded.clone());
                    self.rt.spawn(async move {
                        let tracks = library::tracks(&session, &uri).await;
                        tx.send((uri, tracks));
                    });
                }
            }
        }
        self.playlists = playlists;
        self.selected_playlist = selected_uri.and_then(|uri| self.playlists.iter().position(|p| p.uri == uri));
        self.load_playlist_covers(ctx);
    }

    fn drain_reload(&mut self) {
        let Ok((uri, tracks)) = self.reload_rx.try_recv() else { return };
        if self.loaded_playlist_uri.as_deref() != Some(uri.as_str()) || tracks.is_empty() {
            return;
        }
        let top_uri = self
            .table_top_row
            .and_then(|row| self.visible_order().get(row).copied())
            .map(|i| self.tracks[i].uri.clone());
        self.tracks = tracks;
        self.order_cache = None;
        if let Some(top_uri) = top_uri {
            let order = self.visible_order();
            self.scroll_to_row = order.iter().position(|&i| self.tracks[i].uri == top_uri);
        }
    }

    fn visible_order(&mut self) -> Vec<usize> {
        let key = OrderKey {
            tracks_ptr: self.tracks.as_ptr() as usize,
            tracks_len: self.tracks.len(),
            query: self.search_query.clone(),
            sort_mode: self.sort_mode,
            meta_rev: self.last_meta_rev,
        };
        if let Some((cached_key, order)) = &self.order_cache {
            if *cached_key == key {
                return order.clone();
            }
        }
        let order = self.compute_order();
        self.order_cache = Some((key, order.clone()));
        order
    }

    fn compute_order(&self) -> Vec<usize> {
        let query = self.search_query.to_lowercase();
        let mut order: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                query.is_empty()
                    || self.tracks[i].name.to_lowercase().contains(&query)
                    || self.tracks[i].artist.to_lowercase().contains(&query)
            })
            .collect();
        match self.sort_mode {
            SortMode::Original => {}
            SortMode::Name => order.sort_by_cached_key(|&i| self.tracks[i].name.to_lowercase()),
            SortMode::Artist => order.sort_by_cached_key(|&i| self.tracks[i].artist.to_lowercase()),
            SortMode::Album => order.sort_by_cached_key(|&i| self.tracks[i].album.to_lowercase()),
            SortMode::Added => order.sort_by(|&a, &b| {
                self.tracks[b].added_at.cmp(&self.tracks[a].added_at)
            }),
            SortMode::Duration => order.sort_by_key(|&i| self.tracks[i].duration),
        }
        order
    }

    fn play_playlist_now(&mut self, playlist_idx: usize) {
        self.selected_playlist = Some(playlist_idx);
        self.load_tracks(playlist_idx);
        let order = self.visible_order();
        if order.is_empty() {
            return;
        }
        let start = self.playback.pick_start_index(order.len());
        self.play_ordered(&order, start);
    }

    fn play_ordered(&mut self, order: &[usize], display_pos: usize) {
        self.queue_playlist_uri = self.loaded_playlist_uri.clone();
        let tracks: Vec<QueueTrack> = order.iter().map(|&i| queue_track_from_ref(&self.tracks[i])).collect();
        self.playback.load_queue(&tracks, display_pos);
    }
}

impl eframe::App for App {
    /// El orden de los paneles importa: el primero en cada eje reclama su franja completa.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        image_cache::begin_frame(ctx);
        self.schedule_repaint(ctx);

        self.handle_shortcuts(ctx);
        self.update_min_window_size(ctx);
        self.persist_ui_state(ctx);

        let settings = self.settings_ui.show(ctx, self.ui_scale);
        if let Some(scale) = settings.new_ui_scale {
            self.set_ui_scale(ctx, scale);
        }
        if let Some(source) = settings.new_theme_source {
            theme::set_source(ctx, source);
            self.save_settings();
        }
        if settings.logged_out {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if settings.deepl_changed {
            self.translation_needs_refresh = true;
        }
        if settings.user_names_cleared {
            let shown = match self.nav.last() {
                Some(browse_view::Screen::Playlist(p)) => Some(p.clone()),
                _ => self.selected_playlist.and_then(|i| self.playlists.get(i)).cloned(),
            };
            if let Some(playlist) = shown {
                self.resolve_owner_name_for(&playlist);
            }
        }

        self.update_cover(ctx);
        self.update_cover_large(ctx);
        self.update_lyrics();
        self.update_translation();
        self.maybe_save_resume_state(ctx);
        self.drain_actions();
        self.drain_search();
        self.drain_browse();
        self.drain_playlists(ctx);
        self.drain_reload();
        self.sync_backfilled_artists();
        self.show_toast(ctx);

        egui::TopBottomPanel::top("search_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            egui::Frame::default()
                .fill(theme::card_fill())
                .rounding(theme::rounding())
                .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("🔍");
                        // se restan ~40 px para el botón de ajustes: con `f32::INFINITY` no queda lugar.
                        let text_width = (ui.available_width() - 40.0).max(50.0);
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .id(search_box_id())
                                .desired_width(text_width)
                                .hint_text("Filtrar la playlist actual, o Enter para buscar en Spotify")
                                .frame(false),
                        );
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.run_global_search();
                        }
                        if ui.selectable_label(self.settings_ui.open, "⚙").on_hover_text("Ajustes").clicked() {
                            self.settings_ui.open = !self.settings_ui.open;
                        }
                    });
                });
            ui.add_space(6.0);
        });

        // el panel de abajo va primero para ocupar todo el ancho de la ventana.
        let own_playlists = self.own_playlists();
        let accent = theme::accent_color();
        let mut bar_action: Option<RowAction> = None;
        egui::TopBottomPanel::bottom("now_playing").show(ctx, |ui| {
            let st = self.playback.status.lock().unwrap().clone();

            ui.add_space(6.0);

            if let Some(target) = seek_bar(ui, st.position, st.duration) {
                self.playback.seek_to(target);
            }

            ui.add_space(8.0);

            // 3 columnas (la tercera vacía) para centrar de verdad; otro elemento interactivo en esta fila infla la altura del panel y aplasta playlists/tracks.
            let title = st.title.clone().unwrap_or_default();
            let artist = st.artist.clone().unwrap_or_default();
            ui.columns(3, |columns| {
                columns[0].horizontal(|ui| {
                    if let Some(texture) = &self.cover_texture {
                        ui.add(
                            egui::Image::new(texture)
                                .max_size(egui::vec2(40.0, 40.0))
                                .rounding(theme::rounding() * 0.6),
                        );
                    }
                    // alto fijo: un alto sin acotar en este panel autoajustable ya rompió el layout.
                    let text_w = (ui.available_width() - 84.0).max(40.0);
                    ui.allocate_ui(egui::vec2(text_w, 40.0), |ui| {
                        ui.vertical(|ui| {
                            ui.add_space(2.0);
                            ui.add(egui::Label::new(title).truncate());
                            ui.add(egui::Label::new(egui::RichText::new(artist).weak().small()).truncate());
                        });
                    });
                    if let Some(uri) = st.track_uri.as_deref() {
                        let is_liked = self.liked.contains(uri);
                        let heart = if is_liked { accent } else { ui.visuals().weak_text_color() };
                        let bar_btn = egui::vec2(28.0, 24.0);
                        if ui
                            .add(egui::Button::new(egui::RichText::new("♥").color(heart)).min_size(bar_btn))
                            .on_hover_text(if is_liked { "Quitar de Me gusta" } else { "Agregar a Me gusta" })
                            .clicked()
                        {
                            bar_action = Some(RowAction::ToggleLike(uri.to_string()));
                        }
                        let mut out = Vec::new();
                        egui::menu::menu_custom_button(ui, egui::Button::new("+").min_size(bar_btn), |ui| {
                            track_context_menu(ui, uri, is_liked, &own_playlists, &mut out);
                        });
                        if let Some(action) = out.pop() {
                            bar_action = Some(action);
                        }
                    }
                });

                columns[1].horizontal(|ui| {
                    // `Align::Center` no centra el Frame: el margen sale del ancho de la píldora del frame anterior (converge en 1 frame).
                    let width_id = egui::Id::new("now_playing_pill_width");
                    let remembered_width: f32 = ui.data(|d| d.get_temp(width_id)).unwrap_or(0.0);
                    let offset = ((ui.available_width() - remembered_width) / 2.0).max(0.0);
                    let compact_pill = ui.ctx().screen_rect().width() < 720.0;
                    ui.add_space(offset);

                    let frame_response = egui::Frame::default()
                        .fill(theme::card_fill())
                        .rounding(theme::rounding())
                        .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // ancho fijo: los glifos (🔀/➡, ▶/⏸) miden distinto y movían el centro de la píldora.
                                let btn_size = egui::vec2(28.0, 24.0);
                                let mode_icon = match st.queue_behavior {
                                    QueueBehavior::Sequential => "➡",
                                    QueueBehavior::Random => "🔀",
                                    QueueBehavior::LoopAll => "🔁",
                                    QueueBehavior::LoopTrack => "🔂",
                                };
                                if ui
                                    .add_sized(btn_size, egui::Button::new(mode_icon))
                                    .on_hover_text("orden / aleatorio / bucle")
                                    .clicked()
                                {
                                    self.playback.cycle_queue_behavior();
                                }
                                if ui.add_sized(btn_size, egui::Button::new("⏮")).clicked() {
                                    self.playback.previous();
                                }
                                if ui
                                    .add_sized(btn_size, egui::Button::new(if st.playing { "⏸" } else { "▶" }))
                                    .clicked()
                                {
                                    self.playback.pause_or_resume();
                                }
                                if ui.add_sized(btn_size, egui::Button::new("⏭")).clicked() {
                                    self.playback.next();
                                }

                                ui.add_space(16.0);
                                ui.label("🔊");
                                let mut volume = self.volume();
                                if compact_pill {
                                    ui.spacing_mut().slider_width = 60.0;
                                }
                                let slider = ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false));
                                let percent = format!("{}%", (volume * 100.0).round() as u32);
                                if slider.on_hover_text(&percent).changed() {
                                    self.set_volume(volume);
                                }
                                if !compact_pill {
                                    ui.add_sized(egui::vec2(34.0, 16.0), egui::Label::new(percent));
                                }
                                ui.add_space(8.0);
                                if ui
                                    .add_sized(btn_size, egui::SelectableLabel::new(self.lyrics_visible, "🎤"))
                                    .on_hover_text("Letras")
                                    .clicked()
                                {
                                    self.toggle_lyrics();
                                }
                                if ui
                                    .add_sized(btn_size, egui::SelectableLabel::new(self.now_playing_open, "☰"))
                                    .on_hover_text(if self.now_playing_open && !App::now_playing_fits(ui.ctx()) {
                                        "Panel Reproduciendo (Ctrl+R) — falta espacio: agranda la ventana"
                                    } else {
                                        "Panel Reproduciendo (Ctrl+R)"
                                    })
                                    .clicked()
                                {
                                    self.toggle_now_playing();
                                }
                            });
                        });

                    ui.data_mut(|d| d.insert_temp(width_id, frame_response.response.rect.width()));
                });
            });

            ui.add_space(6.0);
        });

        if let Some(action) = bar_action {
            self.apply_row_action(action);
        }

        // antes de cualquier CentralPanel: los paneles laterales tienen que reclamar su franja primero.
        self.now_playing_panel(ctx);

        if self.lyrics_visible {
            egui::CentralPanel::default().show(ctx, |ui| {
                let title = self.playback.status.lock().unwrap().title.clone();
                ui.heading(title.as_deref().unwrap_or("Letras"));
                ui.add_space(8.0);

                if let Some(msg) = &self.lyrics_status {
                    ui.label(egui::RichText::new(msg).weak());
                    return;
                }
                if let Some(msg) = &self.translation_status {
                    ui.label(egui::RichText::new(format!("traducción: {msg}")).weak().small());
                }

                let position = self.playback.status.lock().unwrap().position;
                let current = self.lyrics.iter().rposition(|l| l.start <= position);

                let center = current.unwrap_or(0);
                let start = center.saturating_sub(1);
                let end = (center + 2).min(self.lyrics.len());

                let footer_height = 70.0;
                let extra_rows = self.translations_visible as u32 + self.pronunciation_visible as u32;
                let per_line = 56.0 + 20.0 * extra_rows as f32;
                let block_height = per_line * (end - start).max(1) as f32;
                let free_space = (ui.available_height() - footer_height - block_height).max(0.0);
                ui.add_space(free_space / 2.0);
                ui.vertical_centered(|ui| {
                    for (i, line) in self.lyrics[start..end].iter().enumerate() {
                        let idx = start + i;
                        let is_current = Some(idx) == current;
                        let text = if line.text.is_empty() { "♪" } else { line.text.as_str() };
                        let rich = if is_current {
                            egui::RichText::new(text).size(32.0).color(theme::accent_color())
                        } else {
                            egui::RichText::new(text).size(24.0).weak()
                        };
                        ui.label(rich);
                        if self.pronunciation_visible {
                            if let Some(pron) = self.pronunciation_lines.get(idx).filter(|t| !t.is_empty()) {
                                let size = if is_current { 20.0 } else { 16.0 };
                                ui.label(egui::RichText::new(pron).size(size).weak());
                            }
                        }
                        if self.translations_visible {
                            if let Some(translated) = self.translated_lines.get(idx).filter(|t| !t.is_empty()) {
                                let size = if is_current { 20.0 } else { 16.0 };
                                ui.label(egui::RichText::new(translated).size(size).weak().italics());
                            }
                        }
                        ui.add_space(if is_current { 18.0 } else { 12.0 });
                    }
                });

                ui.add_space((ui.available_height() - footer_height).max(0.0));
                ui.separator();
                let width_id = egui::Id::new("lyrics_footer_width");
                let remembered: f32 = ui.data(|d| d.get_temp(width_id)).unwrap_or(0.0);
                let mut settings_changed = false;
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - remembered) / 2.0).max(0.0));
                    let content_left = ui.cursor().left();
                    // mismo alto en los tres controles: el emoji 🌐 hacía más alto al primero.
                    let footer_btn = egui::vec2(140.0, 26.0);
                    ui.spacing_mut().interact_size.y = footer_btn.y;

                    if ui
                        .add_sized(footer_btn, toggle_button("🌐 Traducir", self.translations_visible, ui))
                        .on_hover_text("Mostrar la traducción")
                        .clicked()
                    {
                        self.translations_visible = !self.translations_visible;
                        if self.translations_visible {
                            self.translation_needs_refresh = true;
                        }
                        settings_changed = true;
                    }

                    egui::ComboBox::from_id_salt("translate_lang")
                        .width(90.0)
                        .selected_text(translate::language_name(&self.translate_lang))
                        .show_ui(ui, |ui| {
                            for (code, name) in translate::LANGUAGES {
                                if ui.selectable_label(self.translate_lang == *code, *name).clicked()
                                    && self.translate_lang != *code
                                {
                                    self.translate_lang = code.to_string();
                                    self.translation_needs_refresh = true;
                                    settings_changed = true;
                                }
                            }
                        })
                        .response
                        .on_hover_text("Idioma de la traducción");

                    ui.add_space(8.0);
                    if ui
                        .add_sized(footer_btn, toggle_button("Aa Pronunciación", self.pronunciation_visible, ui))
                        .on_hover_text("Romaji / pinyin bajo cada línea en japonés o chino")
                        .clicked()
                    {
                        self.pronunciation_visible = !self.pronunciation_visible;
                        settings_changed = true;
                    }
                    ui.data_mut(|d| d.insert_temp(width_id, ui.min_rect().right() - content_left));
                });
                if settings_changed {
                    self.save_settings();
                }
            });
            return;
        }

        // ancho acotado: sin tope este panel no cede al angostar la ventana.
        let playlists_max = (ctx.screen_rect().width() * 0.3).max(160.0);
        egui::SidePanel::left("playlists")
            .width_range(160.0..=playlists_max)
            .show(ctx, |ui| {
                ui.heading("Playlists");
                ui.add_space(4.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let mut clicked = None;
                    let mut double_clicked = None;
                    for (i, p) in self.playlists.iter().enumerate() {
                        let selected = self.selected_playlist == Some(i);

                        // Frame no es clickeable: el `Sense::click()` va con `ui.interact()` sobre su área.
                        let fill = if selected {
                            theme::card_fill()
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let frame = egui::Frame::default()
                            .fill(fill)
                            .rounding(theme::rounding())
                            .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    if let Some(texture) = self.playlist_textures.get(&p.uri) {
                                        ui.add(egui::Image::new(texture).max_size(egui::vec2(32.0, 32.0)).rounding(4.0));
                                    } else {
                                        ui.add_space(32.0);
                                    }
                                    ui.vertical(|ui| {
                                        ui.add(egui::Label::new(&p.name).truncate());
                                        if p.track_count > 0 {
                                            ui.label(
                                                egui::RichText::new(format!("{} canciones", p.track_count))
                                                    .weak()
                                                    .small(),
                                            );
                                        }
                                    });
                                });
                            });
                        let response = ui.interact(frame.response.rect, ui.id().with(("playlist", i)), egui::Sense::click());
                        if response.double_clicked() {
                            double_clicked = Some(i);
                        } else if response.clicked() {
                            clicked = Some(i);
                        }
                    }
                    if let Some(i) = double_clicked {
                        self.play_playlist_now(i);
                    } else if let Some(i) = clicked {
                        self.selected_playlist = Some(i);
                        self.load_tracks(i);
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.nav.is_empty() {
                if !self.browse_view(ui) {
                    return;
                }
            } else if self.search_active() {
                if !self.search_view(ui) {
                    return;
                }
            } else if let Some(p) = self.selected_playlist.and_then(|i| self.playlists.get(i)).cloned() {
                self.playlist_header(ui, &p);
            } else {
                let label = if self.tracks_label.is_empty() { "Tracks" } else { &self.tracks_label };
                ui.heading(label.to_string());
            }

            ui.add_space(6.0);

            let order = self.visible_order();

            let width = ui.available_width();
            let show_added = width > 820.0;
            let show_album = width > 620.0;
            let show_artist = width > 420.0;

            use egui_extras::{Column, TableBuilder};

            let accent = theme::accent_color();
            let own = self.own_playlists();
            let mut pending: Vec<RowAction> = Vec::new();

            // anchos fijos y no `Column::auto()`: auto mide solo las filas visibles y las columnas saltaban.
            let side_col = (width * 0.24).clamp(90.0, 240.0);
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder().at_least(120.0).clip(true));
            if show_artist {
                table = table.column(Column::exact(side_col).clip(true));
            }
            if show_album {
                table = table.column(Column::exact(side_col).clip(true));
            }
            if show_added {
                table = table.column(Column::exact(88.0));
            }
            table = table.column(Column::exact(52.0)).column(Column::exact(28.0));
            if let Some(row) = self.scroll_to_row.take() {
                table = table.scroll_to_row(row, Some(egui::Align::TOP));
            }
            let mut top_row: Option<usize> = None;

            let mut clicked = None;
            let mut open_artist: Option<browse_view::ArtistTarget> = None;
            table
                .header(22.0, |mut header| {
                    header.col(|ui| {
                        if ui.selectable_label(self.sort_mode == SortMode::Name, "Título").clicked() {
                            self.sort_mode = SortMode::Name;
                        }
                    });
                    if show_artist {
                        header.col(|ui| {
                            if ui.selectable_label(self.sort_mode == SortMode::Artist, "Artista").clicked() {
                                self.sort_mode = SortMode::Artist;
                            }
                        });
                    }
                    if show_album {
                        header.col(|ui| {
                            if ui.selectable_label(self.sort_mode == SortMode::Album, "Álbum").clicked() {
                                self.sort_mode = SortMode::Album;
                            }
                        });
                    }
                    if show_added {
                        header.col(|ui| {
                            if ui.selectable_label(self.sort_mode == SortMode::Added, "Agregado").clicked() {
                                self.sort_mode = SortMode::Added;
                            }
                        });
                    }
                    header.col(|ui| {
                        if ui.selectable_label(self.sort_mode == SortMode::Duration, "⏱").clicked() {
                            self.sort_mode = SortMode::Duration;
                        }
                    });
                    header.col(|_ui| {});
                })
                .body(|body| {
                    body.rows(22.0, order.len(), |mut row| {
                        let display_pos = row.index();
                        top_row = Some(top_row.map_or(display_pos, |r| r.min(display_pos)));
                        let t = &self.tracks[order[display_pos]];
                        let is_liked = self.liked.contains(&t.uri);
                        let mut heart_clicked = false;
                        let mut cells: Vec<egui::Response> = Vec::new();

                        let (_, r) = row.col(|ui| {
                            if show_artist {
                                ui.add(egui::Label::new(&t.name).truncate());
                            } else {
                                ui.add(egui::Label::new(format!("{} — {}", t.name, t.artist)).truncate());
                            }
                        });
                        cells.push(r);
                        if show_artist {
                            let (_, r) = row.col(|ui| {
                                if let Some(target) =
                                    browse_view::artist_links(ui, &t.artists, &t.artist_uris, |s| egui::RichText::new(s).weak())
                                {
                                    open_artist = Some(target);
                                }
                            });
                            cells.push(r);
                        }
                        if show_album {
                            let (_, r) = row.col(|ui| {
                                ui.add(egui::Label::new(egui::RichText::new(&t.album).weak()).truncate());
                            });
                            cells.push(r);
                        }
                        if show_added {
                            let (_, r) = row.col(|ui| {
                                ui.label(egui::RichText::new(t.added_at.as_deref().unwrap_or("—")).weak());
                            });
                            cells.push(r);
                        }
                        let (_, r) = row.col(|ui| {
                            ui.label(egui::RichText::new(fmt_duration(t.duration)).weak());
                        });
                        cells.push(r);

                        let (_, r) = row.col(|ui| {
                            let color = if is_liked { accent } else { accent.gamma_multiply(0.25) };
                            let heart = ui.add(
                                egui::Label::new(egui::RichText::new("♥").color(color)).sense(egui::Sense::click()),
                            );
                            if heart.clicked() {
                                pending.push(RowAction::ToggleLike(t.uri.clone()));
                                heart_clicked = true;
                            }
                        });
                        cells.push(r);

                        for r in cells {
                            r.context_menu(|ui| track_context_menu(ui, &t.uri, is_liked, &own, &mut pending));
                        }
                        if row.response().clicked() && !heart_clicked && open_artist.is_none() {
                            clicked = Some(display_pos);
                        }
                    });
                });
            self.table_top_row = top_row;
            if let Some(target) = open_artist {
                self.open_artist(target);
            } else if let Some(display_pos) = clicked {
                self.play_ordered(&order, display_pos);
            }
            for action in pending {
                self.apply_row_action(action);
            }
        });
    }
}

/// El `File` devuelto tiene que vivir mientras dure el programa: al cerrarse se suelta el lock.
fn acquire_single_instance_lock() -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let dir = format!("{}/.cache/psst-tui", std::env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{dir}/psst-egui.lock"))
        .ok()?;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (result == 0).then_some(file)
}

fn build_app(ctx: &egui::Context) -> App {
    println!("Conectando (librería y audio)...");
    let audio = std::thread::Builder::new().name("audio-connect".into()).spawn(Playback::connect).expect("no se pudo crear el hilo audio-connect");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("library-rt")
        .build()
        .unwrap();
    let session = rt.block_on(library::connect());
    let cached = library::cached_playlists(&session.username());
    let from_cache = cached.is_some();
    let (playlists, liked) = match cached {
        Some(list) => (Some(list), rt.block_on(library::liked_uri_set(&session))),
        None => rt.block_on(async {
            let (list, liked) = tokio::join!(library::refresh_playlists(&session), library::liked_uri_set(&session));
            if let Some(list) = &list {
                cover::store_playlist_covers(&session, list).await;
            }
            (list, liked)
        }),
    };
    let fresh = playlists.is_some() && !from_cache;
    let playlists = playlists.unwrap_or_else(|| vec![library::liked_playlist(session.username())]);
    let playback = audio
        .join()
        .unwrap_or_else(|p| panic!("audio: {}", panic_message(p.as_ref())));
    mpris::spawn(playback.clone(), session.clone());
    rt.spawn(library::backfill_artists_loop(session.clone(), playback.clone()));

    let (action_tx, action_rx) = std::sync::mpsc::channel();
    let action_tx = UiSender::new(action_tx, ctx);
    let (thumb_tx, thumb_rx) = std::sync::mpsc::channel();
    let (search_tx, search_rx) = std::sync::mpsc::channel();
    let search_tx = UiSender::new(search_tx, ctx);
    let (browse_tx, browse_rx) = std::sync::mpsc::channel();
    let browse_tx = UiSender::new(browse_tx, ctx);
    let (playlists_tx, playlists_rx) = std::sync::mpsc::channel();
    let playlists_tx = UiSender::new(playlists_tx, ctx);
    let (reload_tx, reload_rx) = std::sync::mpsc::channel();
    let reload_tx = UiSender::new(reload_tx, ctx);
    let saved_settings = settings::load();
    let mut app = App {
        rt,
        session,
        playback,
        playlists,
        selected_playlist: None,
        tracks: Vec::new(),
        loaded_playlist_uri: None,
        tracks_label: String::new(),
        search_query: String::new(),
        sort_mode: SortMode::Original,
        playlist_textures: HashMap::new(),
        cover_texture: None,
        cover_shown: None,
        owner_name: None,
        lyrics_visible: false,
        lyrics: Vec::new(),
        lyrics_track_uri: None,
        lyrics_status: None,
        lyrics_needs_refresh: false,
        translations_visible: saved_settings.translations_visible,
        translated_lines: Vec::new(),
        translation_track_uri: None,
        translation_status: None,
        translation_needs_refresh: false,
        translate_lang: saved_settings.translate_lang.clone(),
        system_volume: SystemVolume::spawn("psst-egui"),
        pronunciation_visible: saved_settings.pronunciation_visible,
        pronunciation_lines: Vec::new(),
        settings_ui: settings_ui::SettingsUi::new(),
        last_resume_save: std::time::Instant::now(),
        last_resume_saved: None,
        order_cache: None,
        liked,
        collection_stale: false,
        action_tx,
        action_rx,
        toast: None,
        search: None,
        search_tx,
        search_rx,
        search_generation: 0,
        nav: Vec::new(),
        browse_tx,
        browse_rx,
        browse_next_id: 0,
        last_meta_rev: 0,
        ui_scale: settings::clamp_ui_scale(saved_settings.ui_scale),
        now_playing_open: saved_settings.now_playing_panel,
        min_size_sent: None,
        queue_playlist_uri: None,
        pending_volume: None,
        window_size: saved_settings.window_size,
        window_pos: saved_settings.window_pos,
        window_maximized: saved_settings.window_maximized,
        saved_settings: saved_settings.clone(),
        last_settings_check: std::time::Instant::now(),
        cover_large_texture: None,
        cover_large_shown: None,
        thumbs: HashMap::new(),
        thumb_tx,
        thumb_rx,
        playlists_rx,
        reload_tx,
        reload_rx,
        table_top_row: None,
        scroll_to_row: None,
    };

    let repaint = {
        let ctx = ctx.clone();
        move || ctx.request_repaint()
    };
    app.playback.set_notifier(std::sync::Arc::new(repaint.clone()));
    app.system_volume.set_notifier(std::sync::Arc::new(repaint));

    if let Some(volume) = saved_settings.volume {
        app.playback.set_volume(volume);
        app.pending_volume = Some(volume);
    }

    let resumed = resume::load().is_some_and(|state| {
        let Some(playlist_idx) = app.playlists.iter().position(|p| p.uri == state.playlist_uri) else {
            return false;
        };
        app.selected_playlist = Some(playlist_idx);
        app.load_tracks(playlist_idx);

        let Some(track_idx) = app.tracks.iter().position(|t| t.uri == state.track_uri) else {
            return false;
        };
        let order = app.visible_order();
        let Some(display_pos) = order.iter().position(|&i| i == track_idx) else {
            return false;
        };

        // silenciado: `load_queue` arranca a sonar desde 0:00 antes del seek y la pausa.
        let real_volume = app.playback.status.lock().unwrap().volume;
        app.playback.set_volume(0.0);

        app.play_ordered(&order, display_pos);

        // psst-core trata un `Pause` antes de que arranque la pista como `Stop`.
        for _ in 0..50 {
            if app.playback.status.lock().unwrap().track_uri.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let target = std::time::Duration::from_millis(state.position_ms);
        app.playback.seek(target);

        // una pausa en medio del seek lo corta: se espera a que se aplique.
        for _ in 0..50 {
            let pos = app.playback.status.lock().unwrap().position;
            if pos.as_millis().abs_diff(target.as_millis()) < 1500 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        app.playback
            .set_queue_behavior(resume::queue_behavior_from_str(&state.queue_behavior));
        app.playback.pause();
        app.playback.set_volume(real_volume);
        true
    });

    let last_idx = saved_settings
        .last_playlist_uri
        .as_deref()
        .and_then(|uri| app.playlists.iter().position(|p| p.uri == uri));
    if let Some(idx) = last_idx {
        app.selected_playlist = Some(idx);
        app.load_tracks(idx);
    } else if !resumed && !app.playlists.is_empty() {
        app.selected_playlist = Some(0);
        app.load_tracks(0);
    }

    app.load_playlist_covers(ctx);

    if from_cache {
        refresh_playlists(&app.rt, app.session.clone(), playlists_tx, &[0, 5, 15, 30]);
    } else if !fresh {
        refresh_playlists(&app.rt, app.session.clone(), playlists_tx, &[5, 15, 30]);
    }
    app
}

/// Arrastrar salta recién al soltar: un seek por frame rebusca el stream decenas de veces por segundo.
fn seek_bar(ui: &mut egui::Ui, position: std::time::Duration, duration: std::time::Duration) -> Option<std::time::Duration> {
    let total = duration.as_secs_f32();
    let label_width = 44.0;
    let height = 16.0;
    let mut target = None;

    ui.horizontal(|ui| {
        // ancho fijo para los tiempos: si no, la barra cambia de largo
        // cuando el texto pasa de "9:59" a "10:00".
        let elapsed_rect = ui.allocate_exact_size(egui::vec2(label_width, height), egui::Sense::hover()).0;
        let bar_width = (ui.available_width() - label_width - ui.spacing().item_spacing.x).max(40.0);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(bar_width, height), egui::Sense::click_and_drag());
        let total_rect = ui.allocate_exact_size(egui::vec2(label_width, height), egui::Sense::hover()).0;

        let enabled = total > 0.0;
        let pointer_ratio = || {
            response
                .interact_pointer_pos()
                .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
        };
        let dragging = enabled && response.dragged();
        let mut ratio = if enabled { (position.as_secs_f32() / total).clamp(0.0, 1.0) } else { 0.0 };
        if dragging {
            if let Some(r) = pointer_ratio() {
                ratio = r;
            }
        }
        if enabled && (response.clicked() || response.drag_stopped()) {
            if let Some(r) = pointer_ratio() {
                target = Some(std::time::Duration::from_secs_f32(r * total));
                ratio = r;
            }
        }

        let visuals = ui.visuals();
        let painter = ui.painter();
        let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 4.0));
        painter.rect_filled(track, 2.0, visuals.widgets.inactive.bg_fill);
        let mut filled = track;
        filled.set_right(track.left() + track.width() * ratio);
        painter.rect_filled(filled, 2.0, theme::accent_color());
        if enabled && (response.hovered() || dragging) {
            painter.circle_filled(egui::pos2(filled.right(), track.center().y), 6.0, visuals.text_color());
        }

        let shown = if enabled { std::time::Duration::from_secs_f32(ratio * total) } else { std::time::Duration::ZERO };
        let font = egui::TextStyle::Small.resolve(ui.style());
        let color = visuals.weak_text_color();
        painter.text(elapsed_rect.right_center(), egui::Align2::RIGHT_CENTER, fmt_duration(shown), font.clone(), color);
        painter.text(total_rect.left_center(), egui::Align2::LEFT_CENTER, fmt_duration(duration), font, color);

        if enabled {
            response.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    });
    target
}

const MIN_LAYOUT: egui::Vec2 = egui::vec2(560.0, 440.0);
const NOW_PLAYING_MIN_WIDTH: f32 = 260.0;

enum QueueRowClick {
    None,
    Play,
    Artist(browse_view::ArtistTarget),
}

pub(crate) struct UiSender<T> {
    tx: std::sync::mpsc::Sender<T>,
    ctx: egui::Context,
}

// a mano: `derive` le pediría `T: Clone`, que no hace falta
impl<T> Clone for UiSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            ctx: self.ctx.clone(),
        }
    }
}

impl<T> UiSender<T> {
    fn new(tx: std::sync::mpsc::Sender<T>, ctx: &egui::Context) -> Self {
        Self { tx, ctx: ctx.clone() }
    }

    pub(crate) fn send(&self, msg: T) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

fn refresh_playlists(
    rt: &tokio::runtime::Runtime,
    session: librespot_core::session::Session,
    tx: UiSender<Vec<PlaylistRef>>,
    waits: &'static [u64],
) {
    rt.spawn(async move {
        for &wait in waits {
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            if let Some(playlists) = library::refresh_playlists(&session).await {
                cover::store_playlist_covers(&session, &playlists).await;
                tx.send(playlists);
                return;
            }
        }
        eprintln!("playlists: no se pudieron actualizar; queda la lista que había");
    });
}

fn queue_track_from_ref(t: &TrackRef) -> QueueTrack {
    QueueTrack {
        uri: t.uri.clone(),
        title: t.name.clone(),
        artists: t.artists.clone(),
        artist_uris: t.artist_uris.clone(),
        duration: t.duration,
        album: t.album.clone(),
        year: t.year,
        cover: t.cover,
        cover_large: t.cover_large,
    }
}

fn track_ref_from_hit(h: &search::TrackHit) -> TrackRef {
    // sin el artista en el nombre: la lista ya arma "artista — nombre".
    TrackRef {
        name: h.name.clone(),
        uri: h.uri.clone(),
        duration: h.duration,
        artist: h.artist.clone(),
        artists: h.artists.clone(),
        artist_uris: h.artist_uris.clone(),
        album: h.album.clone(),
        year: h.year,
        cover: None,
        cover_large: None,
        added_at: None,
    }
}

fn debug_repaint(ctx: &egui::Context) {
    static START: std::sync::OnceLock<Option<std::time::Instant>> = std::sync::OnceLock::new();
    let Some(start) = START.get_or_init(|| {
        std::env::var("PSST_DEBUG_REPAINT")
            .is_ok_and(|v| v == "1")
            .then(std::time::Instant::now)
    }) else {
        return;
    };
    let causes = ctx.repaint_causes();
    let t = start.elapsed().as_secs_f64();
    if causes.is_empty() {
        eprintln!("[{t:9.3}s] frame — entrada (mouse/teclado/ventana)");
    } else {
        let list: Vec<String> = causes.iter().map(|c| format!("{c:?}")).collect();
        eprintln!("[{t:9.3}s] frame — {}", list.join(", "));
    }
}

fn search_box_id() -> egui::Id {
    egui::Id::new("search_box")
}

fn toggle_button(text: &str, on: bool, ui: &egui::Ui) -> egui::Button<'static> {
    let label = if on {
        egui::RichText::new(text).color(ui.visuals().selection.stroke.color)
    } else {
        egui::RichText::new(text)
    };
    egui::Button::new(label).selected(on)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "error desconocido".to_string())
}

enum Startup {
    NeedLogin,
    Status(String),
    LoginUrl(String),
    Failed(String),
    Ready(Box<App>),
}

enum StartupCmd {
    Login,
    Retry,
    Relogin,
}

fn startup_thread(ctx: egui::Context, tx: std::sync::mpsc::Sender<Startup>, cmd_rx: std::sync::mpsc::Receiver<StartupCmd>) {
    let send = |m: Startup| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };
    loop {
        if !login::has_credentials() {
            send(Startup::NeedLogin);
            match cmd_rx.recv() {
                Ok(StartupCmd::Login | StartupCmd::Relogin | StartupCmd::Retry) => {}
                Err(_) => return,
            }
            send(Startup::Status("Completa el login en el navegador…".to_string()));
            let pending = login::PendingLogin::begin();
            send(Startup::LoginUrl(pending.url.clone()));
            let _ = open::that(&pending.url);
            if let Err(e) = pending.finish() {
                send(Startup::Failed(e));
                match cmd_rx.recv() {
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
        }

        let status = if library::has_cached_credentials() {
            "Conectando…"
        } else {
            "Autoriza la librería en el navegador (paso 2 de 2)…"
        };
        send(Startup::Status(status.to_string()));
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_app(&ctx)));
        match built {
            Ok(app) => {
                send(Startup::Ready(Box::new(app)));
                return;
            }
            Err(payload) => {
                send(Startup::Failed(panic_message(payload.as_ref())));
                match cmd_rx.recv() {
                    Ok(StartupCmd::Relogin) => login::logout(),
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
    }
}

struct Shell {
    app: Option<App>,
    rx: std::sync::mpsc::Receiver<Startup>,
    cmd_tx: std::sync::mpsc::Sender<StartupCmd>,
    status: String,
    login_url: Option<String>,
    need_login: bool,
    error: Option<String>,
}

impl Shell {
    fn command(&mut self, cmd: StartupCmd) {
        self.need_login = false;
        self.error = None;
        let _ = self.cmd_tx.send(cmd);
    }
}

impl eframe::App for Shell {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(app) = &mut self.app {
            app.save_settings();
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        debug_repaint(ctx);
        while self.app.is_none() {
            match self.rx.try_recv() {
                Ok(Startup::Ready(app)) => self.app = Some(*app),
                Ok(Startup::NeedLogin) => self.need_login = true,
                Ok(Startup::Status(s)) => {
                    self.status = s;
                    self.error = None;
                }
                Ok(Startup::LoginUrl(u)) => self.login_url = Some(u),
                Ok(Startup::Failed(e)) => self.error = Some(e),
                Err(_) => break,
            }
        }
        if let Some(app) = &mut self.app {
            app.update(ctx, frame);
            return;
        }

        let mut command = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 70.0).max(20.0));
                if let Some(err) = &self.error {
                    ui.label(egui::RichText::new("No se pudo conectar").size(20.0));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(err).weak());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() / 2.0 - 130.0).max(0.0));
                        if ui.button("Reintentar").clicked() {
                            command = Some(StartupCmd::Retry);
                        }
                        if ui.button("Iniciar sesión de nuevo").clicked() {
                            command = Some(StartupCmd::Relogin);
                        }
                    });
                } else if self.need_login {
                    ui.label(egui::RichText::new("Bienvenido a Psst").size(22.0));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Inicia sesión con tu cuenta de Spotify para empezar.").weak());
                    ui.add_space(12.0);
                    if ui.button("Iniciar sesión con Spotify").clicked() {
                        command = Some(StartupCmd::Login);
                    }
                } else {
                    let msg = if self.status.is_empty() { "Conectando…" } else { &self.status };
                    ui.label(egui::RichText::new(msg).size(20.0).weak());
                    if let Some(url) = &self.login_url {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Si el navegador no se abrió, copia esta dirección:").weak().small());
                        ui.add(egui::Label::new(egui::RichText::new(url).small().monospace()).truncate());
                        if ui.small_button("Copiar dirección").clicked() {
                            ui.output_mut(|o| o.copied_text = url.clone());
                        }
                    }
                }
            });
        });
        if let Some(cmd) = command {
            self.command(cmd);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // `_lock` y no `_`: con `_` el lock se soltaría al final de esta expresión.
    let Some(_instance_lock) = acquire_single_instance_lock() else {
        eprintln!("psst-egui ya está corriendo — no abro una segunda instancia.");
        std::process::exit(0);
    };

    // antes de `Playback::connect` y de crear otros hilos: mutar el entorno es `unsafe`.
    unsafe {
        std::env::set_var("PULSE_PROP_application.name", "Psst");
    }

    psst_backend::housekeeping::spawn_cache_cleanup(2 << 30, 30);

    let saved = settings::load();
    let theme_source = theme::ThemeSource::from_setting(&saved.theme_source);
    let min_size = MIN_LAYOUT * settings::clamp_ui_scale(saved.ui_scale);
    let mut viewport = egui::ViewportBuilder::default()
        // app_id/WM_CLASS igual que `StartupWMClass` del .desktop: sin esto no hay reglas de ventana.
        .with_app_id("psst-egui")
        .with_transparent(true)
        .with_min_inner_size(min_size)
        .with_maximized(saved.window_maximized);
    if let Some(size) = saved.window_size {
        viewport = viewport.with_inner_size(egui::Vec2::from(size).max(min_size));
    }
    if let Some(pos) = saved.window_pos {
        viewport = viewport.with_position(pos);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // sin vsync: tapada por otra ventana, eglSwapBuffers espera un frame callback que no llega y Hyprland marca la app como "no responde".
        vsync: false,
        ..Default::default()
    };

    eframe::run_native(
        "psst-egui",
        native_options,
        Box::new(|cc| {
            theme::init(&cc.egui_ctx, theme_source);
            fonts::install_cjk_font(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // el zoom por teclado propio de egui se apaga: Ctrl +/-/0 los maneja `handle_shortcuts`.
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            cc.egui_ctx.set_zoom_factor(settings::clamp_ui_scale(settings::load().ui_scale));
            let (tx, rx) = std::sync::mpsc::channel();
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
            let ctx = cc.egui_ctx.clone();
            let _ = std::thread::Builder::new().name("startup".into()).spawn(move || startup_thread(ctx, tx, cmd_rx));
            Ok(Box::new(Shell {
                app: None,
                rx,
                cmd_tx,
                status: String::new(),
                login_url: None,
                need_login: false,
                error: None,
            }))
        }),
    )
}
