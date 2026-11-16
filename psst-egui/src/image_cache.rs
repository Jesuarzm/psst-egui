use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use eframe::egui;

const MAX_IMAGES: usize = 120;
const TRIM_TO: usize = 100;

struct State {
    frame: u64,
    last_used: HashMap<String, u64>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            frame: 0,
            last_used: HashMap::new(),
        })
    })
}

pub fn image(url: &str) -> egui::Image<'_> {
    let mut s = state().lock().unwrap();
    let frame = s.frame;
    match s.last_used.get_mut(url) {
        Some(last) => *last = frame,
        None => {
            s.last_used.insert(url.to_owned(), frame);
        }
    }
    egui::Image::new(url)
}

pub fn begin_frame(ctx: &egui::Context) {
    let mut s = state().lock().unwrap();
    s.frame += 1;
    if s.last_used.len() <= MAX_IMAGES {
        return;
    }
    let visible_since = s.frame.saturating_sub(1);
    let mut old: Vec<(u64, String)> = s
        .last_used
        .iter()
        .filter(|(_, &frame)| frame < visible_since)
        .map(|(url, &frame)| (frame, url.clone()))
        .collect();
    old.sort_unstable();
    let excess = s.last_used.len() - TRIM_TO;
    for (_, url) in old.into_iter().take(excess) {
        s.last_used.remove(&url);
        ctx.forget_image(&url);
    }
}
