use std::sync::Arc;

use eframe::egui;
use psst_backend::search::{self, AlbumHit, ArtistHit, PlaylistHit, SearchKind, SearchPage, TrackHit};

use crate::{
    browse_view::{artist_links_at, paint_play_button, ArtistTarget, Screen},
    fmt_duration, image_cache, queue_track_from_ref, theme, track_ref_from_hit, App,
};

/// el backend lo recorta a 10 si la app está en modo Development.
const SEARCH_PAGE_SIZE: usize = 20;
const SUMMARY_TRACKS: usize = 5;
const CARD_WIDTH: f32 = 156.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    All,
    Tracks,
    Artists,
    Albums,
    Playlists,
}

impl Chip {
    pub const ALL: [Chip; 5] = [Chip::All, Chip::Tracks, Chip::Artists, Chip::Albums, Chip::Playlists];

    fn label(self) -> &'static str {
        match self {
            Chip::All => "Todo",
            Chip::Tracks => "Canciones",
            Chip::Artists => "Artistas",
            Chip::Albums => "Álbumes",
            Chip::Playlists => "Playlists",
        }
    }
}

enum Best {
    None,
    Track,
    Artist {
        artist: ArtistHit,
        top: Option<Result<Vec<TrackHit>, String>>,
    },
}

pub struct SearchState {
    query: String,
    generation: u64,
    page: Option<SearchPage>,
    pub chip: Chip,
    best: Best,
    loading: Option<Option<SearchKind>>,
    error: Option<String>,
}

impl SearchState {
    pub fn tracks(&self) -> &[TrackHit] {
        self.page.as_ref().map_or(&[], |p| &p.tracks.items)
    }
}

pub enum SearchMsg {
    Results {
        generation: u64,
        more: Option<SearchKind>,
        result: Result<SearchPage, String>,
    },
    TopTracks {
        generation: u64,
        result: Result<Vec<TrackHit>, String>,
    },
}

enum Action {
    Chip(Chip),
    More(SearchKind),
    PlayResult(usize),
    PlayTop(usize),
    PlayBest,
    OpenArtist(ArtistTarget),
    OpenAlbum(AlbumHit),
    OpenPlaylist(PlaylistHit),
}

pub(crate) fn webapi_token() -> Result<String, String> {
    search::webapi_token_if_available()
        .ok_or_else(|| "la búsqueda no está conectada — configúrala en Ajustes (⚙)".to_string())
}

fn pick_best(query: &str, page: &SearchPage) -> Best {
    let wanted = search::normalize_name(query);
    match page.artists.items.first() {
        Some(artist) if search::normalize_name(&artist.name) == wanted => Best::Artist {
            artist: artist.clone(),
            top: None,
        },
        _ if !page.tracks.items.is_empty() => Best::Track,
        _ => Best::None,
    }
}

impl App {
    pub(crate) fn search_active(&self) -> bool {
        self.search.is_some() && self.selected_playlist.is_none()
    }

    pub(crate) fn run_global_search(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.search_generation += 1;
        let generation = self.search_generation;
        self.nav.clear();
        self.search = Some(SearchState {
            query: query.clone(),
            generation,
            page: None,
            chip: Chip::All,
            best: Best::None,
            loading: Some(None),
            error: None,
        });
        self.selected_playlist = None;
        self.loaded_playlist_uri = None;
        self.tracks.clear();
        self.tracks_label = format!("Búsqueda: {query}");
        self.search_query.clear();
        self.spawn_search_request(query, &SearchKind::ALL, 0, None, generation);
    }

    fn spawn_search_request(
        &self,
        query: String,
        kinds: &[SearchKind],
        offset: usize,
        more: Option<SearchKind>,
        generation: u64,
    ) {
        let tx = self.search_tx.clone();
        let kinds = kinds.to_vec();
        let _ = std::thread::Builder::new().name("search".into()).spawn(move || {
            let result = webapi_token().and_then(|token| search::search(&token, &query, &kinds, SEARCH_PAGE_SIZE, offset));
            let _ = tx.send(SearchMsg::Results {
                generation,
                more,
                result,
            });
        });
    }

    fn search_more(&mut self, kind: SearchKind) {
        let Some(st) = &mut self.search else { return };
        let Some(page) = &st.page else { return };
        if st.loading.is_some() {
            return;
        }
        let offset = match kind {
            SearchKind::Track => page.tracks.items.len(),
            SearchKind::Artist => page.artists.items.len(),
            SearchKind::Album => page.albums.items.len(),
            SearchKind::Playlist => page.playlists.items.len(),
        };
        st.loading = Some(Some(kind));
        let (query, generation) = (st.query.clone(), st.generation);
        self.spawn_search_request(query, &[kind], offset, Some(kind), generation);
    }

    pub(crate) fn drain_search(&mut self) {
        while let Ok(msg) = self.search_rx.try_recv() {
            let Some(st) = &mut self.search else { continue };
            match msg {
                SearchMsg::Results {
                    generation,
                    more,
                    result,
                } if generation == st.generation => {
                    st.loading = None;
                    let page = match result {
                        Ok(page) => page,
                        Err(e) => {
                            st.error = Some(e);
                            continue;
                        }
                    };
                    st.error = None;
                    match (more, &mut st.page) {
                        (Some(kind), Some(current)) => {
                            // con una playlist abierta encima, `tracks` es la de esa playlist: Atrás la rearma desde la búsqueda
                            let playlist_open = self.nav.iter().any(|s| matches!(s, Screen::Playlist(_)));
                            if kind == SearchKind::Track && !playlist_open {
                                self.tracks.extend(page.tracks.items.iter().map(track_ref_from_hit));
                            }
                            current.merge(page);
                        }
                        _ => {
                            self.tracks = page.tracks.items.iter().map(track_ref_from_hit).collect();
                            st.best = pick_best(&st.query, &page);
                            if let Best::Artist { artist, .. } = &st.best {
                                let (tx, uri, name) = (self.search_tx.clone(), artist.uri.clone(), artist.name.clone());
                                let _ = std::thread::Builder::new().name("search-top".into()).spawn(move || {
                                    let result =
                                        webapi_token().and_then(|token| search::artist_top_tracks(&token, &uri, &name));
                                    let _ = tx.send(SearchMsg::TopTracks { generation, result });
                                });
                            }
                            st.page = Some(page);
                        }
                    }
                }
                SearchMsg::TopTracks { generation, result } if generation == st.generation => {
                    if let Best::Artist { top, .. } = &mut st.best {
                        *top = Some(result);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn search_view(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(st) = &self.search else { return false };
        let mut actions = Vec::new();

        ui.label(egui::RichText::new(format!("Resultados para \"{}\"", st.query)).size(20.0));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (i, chip) in Chip::ALL.into_iter().enumerate() {
                if chip_button(ui, chip.label(), st.chip == chip)
                    .on_hover_text(format!("Ctrl+{}", i + 1))
                    .clicked()
                {
                    actions.push(Action::Chip(chip));
                }
            }
        });
        ui.add_space(8.0);

        if let Some(e) = &st.error {
            ui.label(egui::RichText::new(format!("búsqueda falló: {e}")).color(ui.visuals().error_fg_color));
        }
        let show_table = st.chip == Chip::Tracks;
        match &st.page {
            None if st.loading.is_some() => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Buscando…").weak());
                });
            }
            None => {}
            Some(page) if show_table => {
                if page.tracks.items.is_empty() {
                    ui.label(egui::RichText::new("No hay canciones para esta búsqueda.").weak());
                }
                more_button(ui, st, SearchKind::Track, page.tracks.has_more(), &mut actions);
            }
            Some(page) => {
                egui::ScrollArea::vertical()
                    .id_salt("search_results")
                    .auto_shrink(false)
                    .show(ui, |ui| match st.chip {
                        Chip::All => all_view(ui, st, page, &mut actions),
                        chip => grid_view(ui, st, page, chip, &mut actions),
                    });
            }
        }

        for action in actions {
            self.apply_search_action(action);
        }
        show_table
    }

    fn apply_search_action(&mut self, action: Action) {
        match action {
            Action::Chip(chip) => {
                if let Some(st) = &mut self.search {
                    st.chip = chip;
                }
            }
            Action::More(kind) => self.search_more(kind),
            Action::PlayResult(i) => {
                let order: Vec<usize> = (0..self.tracks.len()).collect();
                self.play_ordered(&order, i);
            }
            Action::PlayBest => match &self.search {
                Some(SearchState {
                    best: Best::Artist { .. },
                    ..
                }) => self.apply_search_action(Action::PlayTop(0)),
                Some(_) => self.apply_search_action(Action::PlayResult(0)),
                None => {}
            },
            Action::OpenArtist(target) => self.open_artist(target),
            Action::OpenAlbum(album) => self.open_album(album),
            Action::OpenPlaylist(playlist) => self.open_search_playlist(playlist),
            Action::PlayTop(i) => {
                let Some(SearchState {
                    best: Best::Artist { top: Some(Ok(top)), .. },
                    ..
                }) = &self.search
                else {
                    return;
                };
                let queue: Vec<_> = top.iter().map(|t| queue_track_from_ref(&track_ref_from_hit(t))).collect();
                self.queue_playlist_uri = None;
                self.playback.load_queue(&queue, i);
            }
        }
    }
}

fn all_view(ui: &mut egui::Ui, st: &SearchState, page: &SearchPage, actions: &mut Vec<Action>) {
    if page.is_empty() {
        ui.label(egui::RichText::new(format!("Sin resultados para \"{}\".", st.query)).weak());
        return;
    }

    if ui.available_width() > 680.0 {
        ui.columns(2, |cols| {
            best_card(&mut cols[0], st, page, actions);
            best_tracks(&mut cols[1], st, page, actions);
        });
    } else {
        best_card(ui, st, page, actions);
        ui.add_space(12.0);
        best_tracks(ui, st, page, actions);
    }

    if matches!(st.best, Best::Artist { .. }) && !page.tracks.items.is_empty() {
        ui.add_space(16.0);
        section_title(ui, "Canciones", Some(Chip::Tracks), actions);
        for (i, t) in page.tracks.items.iter().take(SUMMARY_TRACKS).enumerate() {
            result_row(ui, t, Action::PlayResult(i), actions);
        }
    }

    if !page.artists.items.is_empty() {
        ui.add_space(16.0);
        section_title(ui, "Artistas", Some(Chip::Artists), actions);
        card_row(ui, "row_artists", |ui| {
            for a in &page.artists.items {
                if media_card(ui, a.image.as_deref(), &a.name, "Artista", true, true).clicked() {
                    actions.push(Action::OpenArtist(a.into()));
                }
            }
        });
    }
    if !page.albums.items.is_empty() {
        ui.add_space(16.0);
        section_title(ui, "Álbumes", Some(Chip::Albums), actions);
        card_row(ui, "row_albums", |ui| {
            for a in &page.albums.items {
                if media_card(ui, a.image.as_deref(), &a.name, &album_subtitle(a), false, true).clicked() {
                    actions.push(Action::OpenAlbum(a.clone()));
                }
            }
        });
    }
    if !page.playlists.items.is_empty() {
        ui.add_space(16.0);
        section_title(ui, "Playlists", Some(Chip::Playlists), actions);
        card_row(ui, "row_playlists", |ui| {
            for p in &page.playlists.items {
                if media_card(ui, p.image.as_deref(), &p.name, &playlist_subtitle(p), false, true).clicked() {
                    actions.push(Action::OpenPlaylist(p.clone()));
                }
            }
        });
    }
}

fn grid_view(ui: &mut egui::Ui, st: &SearchState, page: &SearchPage, chip: Chip, actions: &mut Vec<Action>) {
    let (empty, kind, has_more) = match chip {
        Chip::Artists => (page.artists.items.is_empty(), SearchKind::Artist, page.artists.has_more()),
        Chip::Albums => (page.albums.items.is_empty(), SearchKind::Album, page.albums.has_more()),
        _ => (page.playlists.items.is_empty(), SearchKind::Playlist, page.playlists.has_more()),
    };
    if empty {
        ui.label(egui::RichText::new(format!("No hay {} para esta búsqueda.", chip.label().to_lowercase())).weak());
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
        match chip {
            Chip::Artists => page.artists.items.iter().for_each(|a| {
                if media_card(ui, a.image.as_deref(), &a.name, "Artista", true, true).clicked() {
                    actions.push(Action::OpenArtist(a.into()));
                }
            }),
            Chip::Albums => page.albums.items.iter().for_each(|a| {
                if media_card(ui, a.image.as_deref(), &a.name, &album_subtitle(a), false, true).clicked() {
                    actions.push(Action::OpenAlbum(a.clone()));
                }
            }),
            _ => page.playlists.items.iter().for_each(|p| {
                if media_card(ui, p.image.as_deref(), &p.name, &playlist_subtitle(p), false, true).clicked() {
                    actions.push(Action::OpenPlaylist(p.clone()));
                }
            }),
        }
    });
    ui.add_space(8.0);
    more_button(ui, st, kind, has_more, actions);
}

fn best_card(ui: &mut egui::Ui, st: &SearchState, page: &SearchPage, actions: &mut Vec<Action>) {
    section_title(ui, "Mejor resultado", None, actions);
    let (image, title, subtitle, round) = match &st.best {
        Best::Artist { artist, .. } => (artist.image.as_deref(), artist.name.clone(), "Artista".to_string(), true),
        Best::Track => {
            let t = &page.tracks.items[0];
            (t.image.as_deref(), t.name.clone(), format!("Canción · {}", t.artist), false)
        }
        Best::None => return,
    };
    let frame = egui::Frame::default()
        .fill(theme::card_fill())
        .rounding(theme::rounding())
        .inner_margin(egui::Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let size = 96.0;
            image_or_placeholder(ui, image, egui::vec2(size, size), if round { size / 2.0 } else { 8.0 });
            ui.add_space(10.0);
            ui.add(egui::Label::new(egui::RichText::new(title).size(26.0)).truncate());
            ui.label(egui::RichText::new(subtitle).weak());
        });
    let card = frame.response.rect;
    let response = ui
        .interact(card, ui.id().with("best_card"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    // se registra después de la tarjeta para quedar encima y que su clic no abra al artista.
    let can_play = match &st.best {
        Best::Artist { top, .. } => matches!(top, Some(Ok(t)) if !t.is_empty()),
        _ => true,
    };
    let mut play_clicked = false;
    if can_play && ui.rect_contains_pointer(card) {
        let radius = 24.0;
        let center = card.right_bottom() - egui::vec2(16.0 + radius, 16.0 + radius);
        let button_rect = egui::Rect::from_center_size(center, egui::vec2(radius * 2.0, radius * 2.0));
        let tip = if matches!(st.best, Best::Artist { .. }) { "Reproducir populares" } else { "Reproducir" };
        let button = ui
            .interact(button_rect, ui.id().with("best_play"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tip);
        paint_play_button(ui, center, radius, button.hovered());
        play_clicked = button.clicked();
    }
    if play_clicked {
        actions.push(Action::PlayBest);
    } else if response.clicked() {
        match &st.best {
            Best::Artist { artist, .. } => actions.push(Action::OpenArtist(artist.into())),
            _ => actions.push(Action::PlayResult(0)),
        }
    }
}

fn result_row(ui: &mut egui::Ui, t: &TrackHit, play: Action, actions: &mut Vec<Action>) {
    let (row, artist) = song_row(ui, t);
    if let Some(target) = artist {
        actions.push(Action::OpenArtist(target));
    } else if row.clicked() {
        actions.push(play);
    }
}

fn best_tracks(ui: &mut egui::Ui, st: &SearchState, page: &SearchPage, actions: &mut Vec<Action>) {
    match &st.best {
        Best::Artist { top, .. } => {
            section_title(ui, "Populares", None, actions);
            match top {
                None => {
                    ui.spinner();
                }
                Some(Err(e)) => {
                    ui.label(egui::RichText::new(e).weak().small());
                }
                Some(Ok(tracks)) if tracks.is_empty() => {
                    ui.label(egui::RichText::new("Sin canciones populares.").weak());
                }
                Some(Ok(tracks)) => {
                    for (i, t) in tracks.iter().enumerate() {
                        result_row(ui, t, Action::PlayTop(i), actions);
                    }
                }
            }
        }
        _ if !page.tracks.items.is_empty() => {
            section_title(ui, "Canciones", Some(Chip::Tracks), actions);
            for (i, t) in page.tracks.items.iter().take(SUMMARY_TRACKS).enumerate() {
                result_row(ui, t, Action::PlayResult(i), actions);
            }
        }
        _ => {}
    }
}

fn chip_button(ui: &mut egui::Ui, text: &str, selected: bool) -> egui::Response {
    let visuals = ui.visuals();
    let (fill, color) = if selected {
        (theme::accent_color(), visuals.selection.stroke.color)
    } else {
        (visuals.widgets.inactive.weak_bg_fill, visuals.text_color())
    };
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(color))
            .fill(fill)
            .rounding(egui::Rounding::same(14.0))
            .min_size(egui::vec2(0.0, 28.0)),
    )
}

fn section_title(ui: &mut egui::Ui, title: &str, see_all: Option<Chip>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(18.0).color(ui.visuals().text_color()));
        if let Some(chip) = see_all {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.link("Ver todo").clicked() {
                    actions.push(Action::Chip(chip));
                }
            });
        }
    });
    ui.add_space(4.0);
}

fn more_button(ui: &mut egui::Ui, st: &SearchState, kind: SearchKind, has_more: bool, actions: &mut Vec<Action>) {
    if st.loading == Some(Some(kind)) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Cargando…").weak());
        });
    } else if has_more && ui.button("Ver más").clicked() {
        actions.push(Action::More(kind));
    }
}

fn album_subtitle(a: &search::AlbumHit) -> String {
    let artists = a.artists.join(", ");
    match a.year {
        Some(year) => format!("{year} · {artists}"),
        None => artists,
    }
}

fn playlist_subtitle(p: &search::PlaylistHit) -> String {
    if p.owner.is_empty() {
        "Playlist".to_string()
    } else {
        format!("De {}", p.owner)
    }
}

fn card_row(ui: &mut egui::Ui, id: &str, add_cards: impl FnOnce(&mut egui::Ui)) {
    egui::ScrollArea::horizontal()
        .id_salt(id)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                add_cards(ui);
            });
        });
}

fn image_or_placeholder(ui: &mut egui::Ui, url: Option<&str>, size: egui::Vec2, rounding: f32) {
    match url {
        Some(url) => {
            ui.add(image_cache::image(url).fit_to_exact_size(size).rounding(rounding));
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            paint_placeholder(ui, rect, rounding);
        }
    }
}

pub(crate) fn paint_placeholder(ui: &egui::Ui, rect: egui::Rect, rounding: f32) {
    ui.painter().rect_filled(rect, rounding, ui.visuals().faint_bg_color);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "♪",
        egui::FontId::proportional(rect.height() / 3.0),
        ui.visuals().weak_text_color(),
    );
}

fn line(ui: &egui::Ui, text: egui::RichText, width: f32, style: egui::TextStyle) -> Arc<egui::Galley> {
    egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Truncate), width.max(10.0), style)
}

pub(crate) fn media_card(
    ui: &mut egui::Ui,
    image: Option<&str>,
    title: &str,
    subtitle: &str,
    round: bool,
    clickable: bool,
) -> egui::Response {
    let pad = 8.0;
    let side = CARD_WIDTH - 2.0 * pad;
    let sense = if clickable { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(CARD_WIDTH, side + 2.0 * pad + 40.0), sense);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    if clickable && response.hovered() {
        ui.painter().rect_filled(rect, theme::rounding(), ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let image_rect = egui::Rect::from_min_size(rect.min + egui::vec2(pad, pad), egui::vec2(side, side));
    let rounding = if round { side / 2.0 } else { 8.0 };
    match image {
        Some(url) => image_cache::image(url).rounding(rounding).paint_at(ui, image_rect),
        None => paint_placeholder(ui, image_rect, rounding),
    }
    let title_g = line(ui, egui::RichText::new(title), side, egui::TextStyle::Body);
    let sub_g = line(ui, egui::RichText::new(subtitle).small(), side, egui::TextStyle::Small);
    let x = image_rect.left();
    let y = image_rect.bottom() + 6.0;
    let visuals = ui.visuals();
    ui.painter().galley(egui::pos2(x, y), title_g.clone(), visuals.text_color());
    ui.painter().galley(egui::pos2(x, y + title_g.size().y + 2.0), sub_g, visuals.weak_text_color());
    let response = response.on_hover_text(title);
    if clickable {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

pub(crate) fn song_row(ui: &mut egui::Ui, t: &TrackHit) -> (egui::Response, Option<ArtistTarget>) {
    const ROW_H: f32 = 52.0;
    const THUMB: f32 = 40.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return (response, None);
    }
    let hovered = response.hovered();
    let visuals = ui.visuals().clone();
    if hovered {
        ui.painter().rect_filled(rect, theme::rounding() * 0.6, visuals.widgets.hovered.weak_bg_fill);
    }
    let thumb = egui::Rect::from_min_size(rect.left_center() + egui::vec2(6.0, -THUMB / 2.0), egui::vec2(THUMB, THUMB));
    match t.thumb.as_deref().or(t.image.as_deref()) {
        Some(url) => image_cache::image(url).rounding(4.0).paint_at(ui, thumb),
        None => paint_placeholder(ui, thumb, 4.0),
    }
    if hovered {
        ui.painter().rect_filled(thumb, 4.0, egui::Color32::from_black_alpha(140));
        ui.painter().text(
            thumb.center(),
            egui::Align2::CENTER_CENTER,
            "▶",
            egui::FontId::proportional(16.0),
            egui::Color32::WHITE,
        );
    }

    let right = rect.right() - 8.0;
    let dur = line(ui, egui::RichText::new(fmt_duration(t.duration)).small(), 80.0, egui::TextStyle::Small);
    let text_left = thumb.right() + 10.0;
    let text_width = right - dur.size().x - 12.0 - text_left;
    let title_color = if hovered { theme::accent_color() } else { visuals.text_color() };
    let title = line(ui, egui::RichText::new(&t.name), text_width, egui::TextStyle::Body);
    let artist_h = ui.text_style_height(&egui::TextStyle::Small);
    let top = rect.center().y - (title.size().y + 2.0 + artist_h) / 2.0;
    let painter = ui.painter();
    painter.galley(egui::pos2(text_left, top), title.clone(), title_color);
    painter.galley(
        egui::pos2(right - dur.size().x, rect.center().y - dur.size().y / 2.0),
        dur,
        visuals.weak_text_color(),
    );
    let artists_rect = egui::Rect::from_min_size(
        egui::pos2(text_left, top + title.size().y + 2.0),
        egui::vec2(text_width.max(10.0), artist_h),
    );
    let weak = visuals.weak_text_color();
    let artist = artist_links_at(ui, artists_rect, &t.artists, &t.artist_uris, |s| {
        egui::RichText::new(s).small().color(weak)
    });
    (response.on_hover_cursor(egui::CursorIcon::PointingHand), artist)
}
