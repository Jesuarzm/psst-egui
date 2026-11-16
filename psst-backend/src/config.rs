use std::env;

fn config_path() -> String {
    format!("{}/.config/Psst/config.json", env::var("HOME").unwrap_or_default())
}

pub fn read() -> serde_json::Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn write(config: &serde_json::Value) {
    let path = config_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, raw);
    }
}

pub fn get_string(key: &str) -> Option<String> {
    read().get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn set_value(key: &str, value: Option<serde_json::Value>) {
    let mut config = read();
    match value {
        Some(v) => config[key] = v,
        None => {
            if let Some(map) = config.as_object_mut() {
                map.remove(key);
            }
        }
    }
    write(&config);
}
