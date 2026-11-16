use std::{
    collections::HashMap,
    env,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::config;

pub const LANGUAGES: &[(&str, &str)] = &[
    ("ES", "Español"),
    ("EN-US", "English"),
    ("PT-BR", "Português"),
    ("FR", "Français"),
    ("DE", "Deutsch"),
    ("IT", "Italiano"),
    ("JA", "日本語"),
    ("KO", "한국어"),
    ("ZH", "中文"),
    ("RU", "Русский"),
];

pub fn language_name(code: &str) -> &str {
    LANGUAGES.iter().find(|(c, _)| *c == code).map_or(code, |(_, n)| n)
}

pub fn deepl_api_key() -> Option<String> {
    config::get_string("deepl_api_key").filter(|k| !k.trim().is_empty())
}

pub fn set_deepl_api_key(key: &str) {
    config::set_value("deepl_api_key", Some(serde_json::Value::String(key.trim().to_string())));
}

pub fn clear_deepl_api_key() {
    config::set_value("deepl_api_key", None);
}

#[derive(Serialize)]
struct DeepLRequest<'a> {
    text: &'a [String],
    target_lang: &'a str,
}

#[derive(Deserialize)]
struct DeepLResponse {
    translations: Vec<DeepLTranslation>,
}

#[derive(Deserialize)]
struct DeepLTranslation {
    text: String,
}

/// Las keys del plan Free terminan en ":fx" y usan el subdominio `api-free`.
fn deepl_endpoint(api_key: &str) -> &'static str {
    if api_key.trim().ends_with(":fx") {
        "https://api-free.deepl.com/v2/translate"
    } else {
        "https://api.deepl.com/v2/translate"
    }
}

/// Traduce en lotes de 50, el máximo de DeepL por request.
fn translate_lines(api_key: &str, lines: &[String], target_lang: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(lines.len());
    for chunk in lines.chunks(50) {
        let body = DeepLRequest { text: chunk, target_lang };
        let response = reqwest::blocking::Client::new()
            .post(deepl_endpoint(api_key))
            .header("Authorization", format!("DeepL-Auth-Key {api_key}"))
            .json(&body)
            .send()
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().unwrap_or_default();
            return Err(format!("DeepL respondió {status}: {text}"));
        }

        let parsed: DeepLResponse = response.json().map_err(|e| e.to_string())?;
        out.extend(parsed.translations.into_iter().map(|t| t.text));
    }
    Ok(out)
}

fn translation_cache_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/lyrics-translations.json")
}

fn translation_cache() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let data = std::fs::read_to_string(translation_cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(data)
    })
}

fn persist_translation_cache(cache: &HashMap<String, Vec<String>>) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(translation_cache_path(), json);
    }
}

pub fn translated_lyrics(
    track_uri: &str,
    target_lang: &str,
    original_lines: &[String],
) -> Result<Vec<String>, String> {
    let cache_key = format!("{track_uri}:{target_lang}");
    if let Some(cached) = translation_cache().lock().unwrap().get(&cache_key) {
        return Ok(cached.clone());
    }

    let api_key = deepl_api_key()
        .ok_or_else(|| "falta la API key de DeepL — agrégala en Ajustes (⚙)".to_string())?;
    let translated = translate_lines(&api_key, original_lines, target_lang)?;

    let mut cache = translation_cache().lock().unwrap();
    cache.insert(cache_key, translated.clone());
    persist_translation_cache(&cache);
    Ok(translated)
}
