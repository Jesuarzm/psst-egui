use eframe::egui;
use psst_backend::{
    library::{self, PlaylistRef},
    search::{self, AlbumHit, ArtistHit, PlaylistHit, Section, TrackHit},
};

use crate::{
    fmt_total_duration, image_cache, queue_track_from_ref,
    search_view::{media_card, paint_placeholder, song_row, webapi_token},
    theme, track_ref_from_hit, App,
};

#[derive(Clone)]
pub struct ArtistTarget {
    pub name: String,
    pub uri: Option<String>,
    pub hit: Option<ArtistHit>,
}

impl From<&ArtistHit> for ArtistTarget {
    fn from(hit: &ArtistHit) -> Self {
        Self {
            name: hit.name.clone(),
            uri: Some(hit.uri.clone()),
            hit: Some(hit.clone()),
        }
    }
}

pub struct ArtistScreen {
    id: u64,
    name: String,
    artist: Option<ArtistHit>,
    top: Option<Result<Vec<TrackHit>, String>>,
    albums: Section<AlbumHit>,
    albums_loading: bool,
    error: Option<String>,
}

pub struct AlbumScreen {
    id: u64,
    album: AlbumHit,
    tracks: Option<Result<Vec<TrackHit>, String>>,
}

pub enum Screen {
    Artist(ArtistScreen),
    Album(AlbumScreen),
    Playlist(PlaylistRef),
}

pub enum BrowseMsg {
    Artist { id: u64, result: Result<ArtistHit, String> },
    Top { id: u64, result: Result<Vec<TrackHit>, String> },
    Albums { id: u64, result: Result<Section<AlbumHit>, String> },
    AlbumTracks { id: u64, result: Result<Vec<TrackHit>, String> },
}

enum Action {
    Back,
    Artist(ArtistTarget),
    Album(AlbumHit),
    MoreAlbums,
    Play(Vec<TrackHit>, usize),
}

impl App {
    fn next_screen_id(&mut self) -> u64 {
        self.browse_next_id += 1;
        self.browse_next_id
    }

    pub(crate) fn open_artist(&mut self, target: ArtistTarget) {
        let id = self.next_screen_id();
        self.nav.push(Screen::Artist(ArtistScreen {
            id,
            name: target.name.clone(),
            artist: target.hit.clone(),
            top: None,
            albums: Section::default(),
            albums_loading: true,
            error: None,
        }));
        let tx = self.browse_tx.clone();
        let _ = std::thread::Builder::new().name("browse-artist".into()).spawn(move || {
            let artist = webapi_token().and_then(|token| {
                let artist = match (&target.hit, &target.uri) {
                    (Some(hit), _) => Ok(hit.clone()),
                    (None, Some(uri)) => search::artist_by_uri(&token, uri),
                    (None, None) => search::find_artist(&token, &target.name),
                };
                artist.map(|a| (token, a))
            });
            let (token, artist) = match artist {
                Ok(ok) => ok,
                Err(e) => {
                    let _ = tx.send(BrowseMsg::Artist { id, result: Err(e) });
                    return;
                }
            };
            let _ = tx.send(BrowseMsg::Artist {
                id,
                result: Ok(artist.clone()),
            });
            let top = search::artist_top_tracks(&token, &artist.uri, &artist.name);
            let _ = tx.send(BrowseMsg::Top { id, result: top });
            let albums = search::artist_albums(&token, &artist.uri, 0);
            let _ = tx.send(BrowseMsg::Albums { id, result: albums });
        });
    }

    pub(crate) fn open_album(&mut self, album: AlbumHit) {
        let id = self.next_screen_id();
        self.nav.push(Screen::Album(AlbumScreen {
            id,
            album: album.clone(),
            tracks: None,
        }));
        let tx = self.browse_tx.clone();
        let _ = std::thread::Builder::new().name("browse-album".into()).spawn(move || {
            let result = webapi_token().and_then(|token| search::album_tracks(&token, &album));
            let _ = tx.send(BrowseMsg::AlbumTracks { id, result });
        });
    }

    pub(crate) fn open_search_playlist(&mut self, hit: PlaylistHit) {
        let Some(mut playlist) = self.rt.block_on(library::playlist_ref(&self.session, &hit.uri)) else {
            self.toast = Some(("No se pudo abrir la playlist".to_string(), std::time::Instant::now()));
            return;
        };
        if playlist.cover_url.is_none() {
            playlist.cover_url = hit.image.clone();
        }
        self.tracks = self.rt.block_on(library::tracks(&self.session, &playlist.uri));
        self.loaded_playlist_uri = Some(playlist.uri.clone());
        self.tracks_label = playlist.name.clone();
        self.resolve_owner_name_for(&playlist);
        self.nav.push(Screen::Playlist(playlist));
    }

    pub(crate) fn go_back(&mut self) {
        if !matches!(self.nav.pop(), Some(Screen::Playlist(_))) {
            return;
        }
        if let Some(search) = &self.search {
            self.tracks = search.tracks().iter().map(track_ref_from_hit).collect();
            self.loaded_playlist_uri = None;
        } else if let Some(idx) = self.selected_playlist {
            self.loaded_playlist_uri = None;
            self.load_tracks(idx);
        }
    }

    pub(crate) fn drain_browse(&mut self) {
        while let Ok(msg) = self.browse_rx.try_recv() {
            let msg_id = match &msg {
                BrowseMsg::Artist { id, .. }
                | BrowseMsg::Top { id, .. }
                | BrowseMsg::Albums { id, .. }
                | BrowseMsg::AlbumTracks { id, .. } => *id,
            };
            let screen = self.nav.iter_mut().find(|s| match s {
                Screen::Artist(a) => a.id == msg_id,
                Screen::Album(a) => a.id == msg_id,
                Screen::Playlist(_) => false,
            });
            match (screen, msg) {
                (Some(Screen::Artist(a)), BrowseMsg::Artist { result, .. }) => match result {
                    Ok(hit) => a.artist = Some(hit),
                    Err(e) => {
                        a.error = Some(e);
                        a.albums_loading = false;
                    }
                },
                (Some(Screen::Artist(a)), BrowseMsg::Top { result, .. }) => a.top = Some(result),
                (Some(Screen::Artist(a)), BrowseMsg::Albums { result, .. }) => {
                    a.albums_loading = false;
                    match result {
                        Ok(more) => {
                            a.albums.total = more.total;
                            a.albums.items.extend(more.items);
                        }
                        Err(e) => a.error = Some(e),
                    }
                }
                (Some(Screen::Album(a)), BrowseMsg::AlbumTracks { result, .. }) => a.tracks = Some(result),
                _ => {}
            }
        }
    }

    pub(crate) fn browse_view(&mut self, ui: &mut egui::Ui) -> bool {
        let mut actions = Vec::new();
        if ui.button("← Atrás").on_hover_text("Alt+←").clicked() {
            actions.push(Action::Back);
        }
        ui.add_space(6.0);

        let show_table = match self.nav.last() {
            Some(Screen::Playlist(playlist)) => {
                let playlist = playlist.clone();
                self.playlist_header(ui, &playlist);
                true
            }
            Some(Screen::Artist(a)) => {
                egui::ScrollArea::vertical()
                    .id_salt(("artist_view", a.id))
                    .auto_shrink(false)
                    .show(ui, |ui| artist_view(ui, a, &mut actions));
                false
            }
            Some(Screen::Album(a)) => {
                egui::ScrollArea::vertical()
                    .id_salt(("album_view", a.id))
                    .auto_shrink(false)
                    .show(ui, |ui| album_view(ui, a, &mut actions));
                false
            }
            None => false,
        };

        for action in actions {
            match action {
                Action::Back => self.go_back(),
                Action::Artist(target) => self.open_artist(target),
                Action::Album(album) => self.open_album(album),
                Action::MoreAlbums => self.more_artist_albums(),
                Action::Play(tracks, start) => {
                    let queue: Vec<_> = tracks.iter().map(|t| queue_track_from_ref(&track_ref_from_hit(t))).collect();
                    self.queue_playlist_uri = None;
                    self.playback.load_queue(&queue, start);
                }
            }
        }
        show_table
    }

    fn more_artist_albums(&mut self) {
        let Some(Screen::Artist(a)) = self.nav.last_mut() else { return };
        let Some(artist) = &a.artist else { return };
        if a.albums_loading {
            return;
        }
        a.albums_loading = true;
        let (id, uri, offset, tx) = (a.id, artist.uri.clone(), a.albums.items.len(), self.browse_tx.clone());
        let _ = std::thread::Builder::new().name("browse-albums".into()).spawn(move || {
            let result = webapi_token().and_then(|token| search::artist_albums(&token, &uri, offset));
            let _ = tx.send(BrowseMsg::Albums { id, result });
        });
    }
}

fn artist_view(ui: &mut egui::Ui, a: &ArtistScreen, actions: &mut Vec<Action>) {
    let top = match &a.top {
        Some(Ok(tracks)) if !tracks.is_empty() => Some(tracks),
        _ => None,
    };
    let image = a.artist.as_ref().and_then(|h| h.image.as_deref());
    if header(ui, image, true, &a.name, "Artista", |_| {}, top.is_some()) {
        if let Some(tracks) = top {
            actions.push(Action::Play(tracks.clone(), 0));
        }
    }
    if let Some(e) = &a.error {
        ui.label(egui::RichText::new(e).color(ui.visuals().error_fg_color));
    }

    ui.add_space(16.0);
    title(ui, "Populares");
    match &a.top {
        None if a.error.is_none() => {
            ui.spinner();
        }
        None => {}
        Some(Err(e)) => {
            ui.label(egui::RichText::new(e).weak().small());
        }
        Some(Ok(tracks)) if tracks.is_empty() => {
            ui.label(egui::RichText::new("Sin canciones populares.").weak());
        }
        Some(Ok(tracks)) => {
            for (i, t) in tracks.iter().enumerate() {
                track_row(ui, t, tracks, i, actions);
            }
        }
    }

    ui.add_space(16.0);
    title(ui, "Discografía");
    if a.albums.items.is_empty() && !a.albums_loading {
        ui.label(egui::RichText::new("Sin álbumes.").weak());
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
        for album in &a.albums.items {
            let subtitle = album.year.map(|y| y.to_string()).unwrap_or_default();
            if media_card(ui, album.image.as_deref(), &album.name, &subtitle, false, true).clicked() {
                actions.push(Action::Album(album.clone()));
            }
        }
    });
    ui.add_space(8.0);
    if a.albums_loading {
        ui.spinner();
    } else if a.albums.has_more() && ui.button("Ver más").clicked() {
        actions.push(Action::MoreAlbums);
    }
}

fn album_view(ui: &mut egui::Ui, a: &AlbumScreen, actions: &mut Vec<Action>) {
    let album = &a.album;
    let tracks = match &a.tracks {
        Some(Ok(tracks)) if !tracks.is_empty() => Some(tracks),
        _ => None,
    };
    let mut meta = String::from("Álbum");
    if let Some(year) = album.year {
        meta.push_str(&format!(" · {year}"));
    }
    if let Some(tracks) = tracks {
        let total: std::time::Duration = tracks.iter().map(|t| t.duration).sum();
        meta.push_str(&format!(" · {} canciones · {}", tracks.len(), fmt_total_duration(total)));
    }
    let artist_uris = tracks.and_then(|t| t.first()).map(|t| t.artist_uris.clone()).unwrap_or_default();
    let mut artist_clicked = None;
    let play = header(
        ui,
        album.image.as_deref(),
        false,
        &album.name,
        &meta,
        |ui| artist_clicked = artist_links(ui, &album.artists, &artist_uris, |s| egui::RichText::new(s).size(15.0)),
        tracks.is_some(),
    );
    if let Some(target) = artist_clicked {
        actions.push(Action::Artist(target));
    }
    if let (true, Some(tracks)) = (play, tracks) {
        actions.push(Action::Play(tracks.clone(), 0));
    }

    ui.add_space(16.0);
    match &a.tracks {
        None => {
            ui.spinner();
        }
        Some(Err(e)) => {
            ui.label(egui::RichText::new(e).color(ui.visuals().error_fg_color));
        }
        Some(Ok(tracks)) => {
            for (i, t) in tracks.iter().enumerate() {
                track_row(ui, t, tracks, i, actions);
            }
        }
    }
}

fn track_row(ui: &mut egui::Ui, t: &TrackHit, all: &[TrackHit], i: usize, actions: &mut Vec<Action>) {
    let (row, artist) = song_row(ui, t);
    if let Some(target) = artist {
        actions.push(Action::Artist(target));
    } else if row.clicked() {
        actions.push(Action::Play(all.to_vec(), i));
    }
}

fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(18.0).color(ui.visuals().text_color()));
    ui.add_space(4.0);
}

fn header(
    ui: &mut egui::Ui,
    image: Option<&str>,
    round: bool,
    name: &str,
    meta: &str,
    extra: impl FnOnce(&mut egui::Ui),
    can_play: bool,
) -> bool {
    let mut play = false;
    let size = if ui.available_width() > 620.0 { 160.0 } else { 96.0 };
    let rounding = if round { size / 2.0 } else { 8.0 };
    ui.horizontal(|ui| {
        match image {
            Some(url) => {
                ui.add(image_cache::image(url).fit_to_exact_size(egui::vec2(size, size)).rounding(rounding));
            }
            None => {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                paint_placeholder(ui, rect, rounding);
            }
        }
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.add_space(size / 2.0 - 40.0);
            ui.add(egui::Label::new(egui::RichText::new(name).size(32.0).color(ui.visuals().text_color())).truncate());
            extra(ui);
            ui.label(egui::RichText::new(meta).weak());
            ui.add_space(6.0);
            if can_play && play_button(ui, 22.0).on_hover_text("Reproducir").clicked() {
                play = true;
            }
        });
    });
    play
}

pub(crate) fn play_button(ui: &mut egui::Ui, radius: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(radius * 2.0, radius * 2.0), egui::Sense::click());
    paint_play_button(ui, rect.center(), radius, response.hovered());
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub(crate) fn paint_play_button(ui: &egui::Ui, center: egui::Pos2, radius: f32, hovered: bool) {
    let r = if hovered { radius * 1.06 } else { radius };
    ui.painter().circle_filled(center, r, theme::accent_color());
    ui.painter().text(
        center + egui::vec2(radius * 0.08, 0.0), // el ▶ se ve corrido a la izquierda si va justo al centro
        egui::Align2::CENTER_CENTER,
        "▶",
        egui::FontId::proportional(radius * 0.9),
        ui.visuals().selection.stroke.color,
    );
}

pub(crate) fn artist_links(
    ui: &mut egui::Ui,
    names: &[String],
    uris: &[String],
    style: impl Fn(&str) -> egui::RichText,
) -> Option<ArtistTarget> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                ui.add(egui::Label::new(style(", ")).selectable(false));
            }
            let response = ui.add(egui::Label::new(style(name)).sense(egui::Sense::click()).selectable(false));
            if response.hovered() {
                let y = response.rect.bottom() - 1.0;
                ui.painter()
                    .hline(response.rect.x_range(), y, egui::Stroke::new(1.0_f32, ui.visuals().text_color()));
            }
            if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                clicked = Some(ArtistTarget {
                    name: name.clone(),
                    uri: uris.get(i).filter(|u| !u.is_empty()).cloned(),
                    hit: None,
                });
            }
        }
    });
    clicked
}

pub(crate) fn artist_links_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    names: &[String],
    uris: &[String],
    style: impl Fn(&str) -> egui::RichText,
) -> Option<ArtistTarget> {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    artist_links(&mut child, names, uris, style)
}
