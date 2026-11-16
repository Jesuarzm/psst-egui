//! Se usa el campo de pronunciación de IPADIC, no el de lectura: así は sale "wa" y へ sale "e".

use std::borrow::Cow;

use lindera::{dictionary::load_dictionary, mode::Mode, segmenter::Segmenter};
use pinyin::ToPinyin;
use wana_kana::ConvertJapanese;

fn is_kana(c: char) -> bool {
    ('\u{3040}'..='\u{30ff}').contains(&c)
}

fn is_han(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
}

pub fn romanize_lines(lines: &[String]) -> Vec<String> {
    let song_has_kana = lines.iter().any(|l| l.chars().any(is_kana));
    let has_cjk = lines.iter().any(|l| l.chars().any(|c| is_kana(c) || is_han(c)));
    if !has_cjk {
        return vec![String::new(); lines.len()];
    }

    let segmenter = song_has_kana
        .then(|| load_dictionary("embedded://ipadic").ok())
        .flatten()
        .map(|dict| Segmenter::new(Mode::Normal, dict, None));

    lines
        .iter()
        .map(|line| {
            let has_kana = line.chars().any(is_kana);
            let has_han = line.chars().any(is_han);
            match (&segmenter, has_kana || has_han) {
                (Some(seg), true) => japanese(seg, line),
                (None, true) if has_han => chinese(line),
                _ => String::new(),
            }
        })
        .collect()
}

fn kana_to_romaji(pron: &str) -> (String, bool) {
    let (body, sokuon) = match pron.strip_suffix('ッ') {
        Some(b) => (b, true),
        None => (pron, false),
    };
    // を se pronuncia "o" (la pronunciación de IPADIC la deja como ヲ)
    let romaji = if body == "ヲ" { "o".to_string() } else { body.to_romaji() };
    let romaji = romaji
        .replace("aa", "ā")
        .replace("uu", "ū")
        .replace("ee", "ē")
        .replace("oo", "ō");
    (romaji, sokuon)
}

fn map_punct(surface: &str) -> String {
    match surface {
        "、" => ",".into(),
        "。" => ".".into(),
        "！" => "!".into(),
        "？" => "?".into(),
        "…" => "...".into(),
        "・" | "　" | "「" | "」" | "『" | "』" => String::new(),
        "（" => "(".into(),
        "）" => ")".into(),
        other => other.to_string(),
    }
}

fn is_closing_punct(s: &str) -> bool {
    matches!(s, "," | "." | "!" | "?" | "..." | ")")
}

fn japanese(seg: &Segmenter, text: &str) -> String {
    let Ok(mut tokens) = seg.segment(Cow::Borrowed(text)) else {
        return String::new();
    };

    let mut out = String::new();
    let mut geminate_next = false;
    let mut prev_pos = String::new();

    for token in tokens.iter_mut() {
        let surface = token.surface.to_string();
        let details: Vec<String> = token.details().iter().map(|d| d.to_string()).collect();
        let pos0 = details.first().map(String::as_str).unwrap_or("");
        let pos1 = details.get(1).map(String::as_str).unwrap_or("");
        // índice 8 = pronunciación en IPADIC; "*" cuando no aplica
        let pron = details.get(8).filter(|d| d.as_str() != "*" && !d.is_empty());

        let (mut piece, sokuon) = match pron {
            Some(p) => kana_to_romaji(p),
            None if surface.chars().all(is_kana) => kana_to_romaji(&surface.to_katakana()),
            None => (map_punct(&surface), false),
        };
        if piece.is_empty() {
            continue;
        }

        if geminate_next {
            if piece.starts_with("ch") {
                piece.insert(0, 't');
            } else if let Some(c) = piece.chars().next() {
                if c.is_ascii_alphabetic() && !"aeiou".contains(c) {
                    piece.insert(0, c);
                }
            }
        }
        geminate_next = sokuon;

        let after_conjugable = matches!(prev_pos.as_str(), "動詞" | "形容詞" | "助動詞");
        let glued = (pos0 == "助動詞" && after_conjugable)
            || (pos0 == "動詞" && pos1 == "非自立")
            || (pos0 == "形容詞" && pos1 == "非自立")
            || (pos0 == "助詞" && pos1 == "接続助詞" && after_conjugable)
            || (pos0 == "名詞" && pos1 == "接尾");

        prev_pos = pos0.to_string();
        if !out.is_empty() && !glued && !is_closing_punct(&piece) {
            out.push(' ');
        }
        out.push_str(&piece);
    }
    out.trim().to_string()
}

fn chinese(text: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut latin = String::new();
    for c in text.chars() {
        if let Some(p) = c.to_pinyin() {
            if !latin.is_empty() {
                parts.push(std::mem::take(&mut latin));
            }
            parts.push(p.with_tone().to_string());
            continue;
        }
        match c {
            '，' | '、' => attach(&mut parts, &mut latin, ","),
            '。' => attach(&mut parts, &mut latin, "."),
            '！' => attach(&mut parts, &mut latin, "!"),
            '？' => attach(&mut parts, &mut latin, "?"),
            ' ' | '　' => {
                if !latin.is_empty() {
                    parts.push(std::mem::take(&mut latin));
                }
            }
            other => latin.push(other),
        }
    }
    if !latin.is_empty() {
        parts.push(latin);
    }
    parts.join(" ")
}

fn attach(parts: &mut Vec<String>, latin: &mut String, punct: &str) {
    if !latin.is_empty() {
        parts.push(std::mem::take(latin));
    }
    match parts.last_mut() {
        Some(last) => last.push_str(punct),
        None => parts.push(punct.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> String {
        romanize_lines(&[line.to_string()]).remove(0)
    }

    #[test]
    fn japanese_particles_and_words() {
        assert_eq!(one("私は学生です"), "watashi wa gakusei desu");
        assert_eq!(one("君の名前を呼んでる"), "kimi no namae o yonderu");
        assert_eq!(one("今日は雨が降っているね"), "kyō wa ame ga futteiru ne");
    }

    #[test]
    fn japanese_long_vowels_and_punctuation() {
        assert_eq!(one("ありがとう、さようなら"), "arigatō, sayōnara");
    }

    #[test]
    fn latin_only_needs_nothing() {
        assert_eq!(one("Hello world"), "");
    }

    #[test]
    fn chinese_gets_pinyin_with_tones() {
        assert_eq!(one("我爱你，你是我的一切"), "wǒ ài nǐ, nǐ shì wǒ de yī qiè");
    }

    #[test]
    fn han_only_line_follows_the_song_language() {
        let out = romanize_lines(&["電光石火".to_string(), "ありがとう".to_string()]);
        assert!(out[0].contains("denkō"), "salió: {}", out[0]);
    }
}
