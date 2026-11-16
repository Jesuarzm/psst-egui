use std::sync::mpsc;

use eframe::egui;
use psst_backend::{login, search, settings, translate};

use crate::theme::{self, ThemeSource};

const DEEPL_URL: &str = "https://www.deepl.com/pro-api";
const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";

#[derive(Default)]
pub struct Outcome {
    pub logged_out: bool,
    pub deepl_changed: bool,
    pub user_names_cleared: bool,
    pub new_ui_scale: Option<f32>,
    pub new_theme_source: Option<ThemeSource>,
}

pub struct SettingsUi {
    pub open: bool,
    deepl_input: String,
    client_id_input: String,
    status: Option<String>,
    connecting: Option<mpsc::Receiver<Result<(), String>>>,
    /// la escala se aplica al soltar: cambiar el zoom en pleno arrastre hace saltar el slider.
    scale_input: f32,
    scale_dragging: bool,
}

impl SettingsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            deepl_input: String::new(),
            client_id_input: search::webapi_client_id().unwrap_or_default(),
            status: None,
            connecting: None,
            scale_input: settings::DEFAULT_UI_SCALE,
            scale_dragging: false,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, ui_scale: f32) -> Outcome {
        let mut outcome = Outcome::default();

        if let Some(rx) = &self.connecting {
            match rx.try_recv() {
                Ok(result) => {
                    // `connect_webapi` ya limpió el cache de nombres.
                    outcome.user_names_cleared = result.is_ok();
                    self.status = Some(match result {
                        Ok(()) => "Búsqueda conectada ✓".to_string(),
                        Err(e) => format!("No se pudo conectar la búsqueda: {e}"),
                    });
                    self.connecting = None;
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(300)),
                Err(mpsc::TryRecvError::Disconnected) => self.connecting = None,
            }
        }
        if !self.open {
            return outcome;
        }

        let mut open = true;
        egui::Window::new("Ajustes")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.heading("Interfaz");
                if !self.scale_dragging {
                    self.scale_input = ui_scale;
                }
                ui.horizontal(|ui| {
                    ui.label("Escala");
                    let slider = ui
                        .add(
                            egui::Slider::new(&mut self.scale_input, settings::MIN_UI_SCALE..=settings::MAX_UI_SCALE)
                                .step_by(settings::UI_SCALE_STEP as f64)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                .custom_parser(|s| s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|p| p / 100.0)),
                        )
                        .on_hover_text("Ctrl + \"+\" / Ctrl + \"-\" / Ctrl + 0");
                    self.scale_dragging = slider.dragged();
                    if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                        outcome.new_ui_scale = Some(self.scale_input);
                    }
                    if ui.button("Restablecer").clicked() {
                        outcome.new_ui_scale = Some(settings::DEFAULT_UI_SCALE);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Tema");
                    let current = theme::source();
                    egui::ComboBox::from_id_salt("theme_source")
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for source in ThemeSource::ALL {
                                if ui.selectable_label(source == current, source.label()).clicked() && source != current {
                                    outcome.new_theme_source = Some(source);
                                }
                            }
                        })
                        .response
                        .on_hover_text("Por defecto: noctalia · DMS: colores del fondo de pantalla · GTK3: gtk.css");
                });
                if let Some(msg) = theme::status() {
                    ui.label(egui::RichText::new(msg).small().color(ui.visuals().warn_fg_color));
                }

                ui.add_space(14.0);
                ui.heading("Cuenta");
                ui.label(egui::RichText::new("Sesión de Spotify iniciada.").weak());
                if ui.button("Cerrar sesión").on_hover_text("Borra el login y cierra la app").clicked() {
                    login::logout();
                    outcome.logged_out = true;
                }

                ui.add_space(14.0);
                ui.heading("Traducción (DeepL)");
                let has_key = translate::deepl_api_key().is_some();
                ui.label(
                    egui::RichText::new(if has_key { "API key guardada ✓" } else { "Sin API key: las letras no se pueden traducir." })
                        .weak(),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.deepl_input)
                            .password(true)
                            .hint_text("Authentication Key (termina en :fx)")
                            .desired_width(250.0),
                    );
                    if ui.add_enabled(!self.deepl_input.trim().is_empty(), egui::Button::new("Guardar")).clicked() {
                        translate::set_deepl_api_key(&self.deepl_input);
                        self.deepl_input.clear();
                        self.status = Some("API key de DeepL guardada ✓".to_string());
                        outcome.deepl_changed = true;
                    }
                    if ui.add_enabled(has_key, egui::Button::new("Borrar")).clicked() {
                        translate::clear_deepl_api_key();
                        outcome.deepl_changed = true;
                    }
                });
                ui.hyperlink_to("Conseguir una key gratis en deepl.com", DEEPL_URL);

                ui.add_space(14.0);
                ui.heading("Búsqueda en Spotify");
                let configured = search::webapi_configured();
                ui.label(
                    egui::RichText::new(if configured { "Conectada ✓" } else { "Sin conectar: solo se filtran tus playlists." }).weak(),
                );
                ui.label("1. Crea una app en el dashboard de Spotify y agrégale esta Redirect URI:");
                ui.horizontal(|ui| {
                    ui.monospace(search::REDIRECT_URI);
                    if ui.small_button("Copiar").clicked() {
                        ui.output_mut(|o| o.copied_text = search::REDIRECT_URI.to_string());
                    }
                });
                ui.hyperlink_to("Abrir el dashboard", DASHBOARD_URL);
                ui.label("2. Pega su Client ID y conecta:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.client_id_input)
                            .hint_text("Client ID")
                            .desired_width(250.0),
                    );
                    let can_connect = self.connecting.is_none() && !self.client_id_input.trim().is_empty();
                    if ui.add_enabled(can_connect, egui::Button::new("Conectar")).clicked() {
                        search::set_webapi_client_id(&self.client_id_input);
                        let (tx, rx) = mpsc::channel();
                        let _ = std::thread::Builder::new().name("webapi-login".into()).spawn(move || {
                            let _ = tx.send(search::connect_webapi(|_| {}));
                        });
                        self.connecting = Some(rx);
                        self.status = Some("Completa el login en el navegador…".to_string());
                    }
                    if ui.add_enabled(configured, egui::Button::new("Desconectar")).clicked() {
                        search::disconnect_webapi();
                        outcome.user_names_cleared = true;
                    }
                });

                ui.add_space(14.0);
                ui.heading("Cache");
                if ui
                    .button("Limpiar nombres de usuario")
                    .on_hover_text("Vuelve a pedir a Spotify los nombres de los dueños de playlists")
                    .clicked()
                {
                    search::clear_user_name_cache();
                    outcome.user_names_cleared = true;
                    self.status = Some("Nombres de usuario borrados ✓".to_string());
                }

                if let Some(msg) = &self.status {
                    ui.add_space(10.0);
                    ui.label(msg);
                }
            });
        self.open = open;
        outcome
    }
}
