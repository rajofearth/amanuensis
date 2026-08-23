use std::{sync::LazyLock, thread, time::{Duration, Instant}};

use arboard::{Clipboard, ImageData};
use enigo::{
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Settings,
};
use regex::Regex;

use crate::log;
use crate::win_focus::{self, FocusTarget};

pub const MIN_AUDIO_SECS: f32 = 0.3;
const RESTORE_DELAY: Duration = Duration::from_millis(500);
const PASTE_SETTLE: Duration = Duration::from_millis(50);
const REPLACEMENTS: &str = "uh:;um:;uhm:;umm:;uhh:;ah:;eh:;hmm:;hm:;mm:;mhm:;mm-hmm:;mmhmm:";

static REPLACE_RULES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    REPLACEMENTS
        .split(';')
        .filter_map(|part| part.split_once(':').map(|(src, _)| src.trim()))
        .filter(|src| !src.is_empty())
        .map(|src| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(src))))
        .collect::<Result<Vec<_>, _>>()
        .expect("valid replacement patterns")
});

static FILLER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[\s,])(uh+|u+m+|uhm|umm+|uhh+|ah+|eh+|hmm+|hm+|m+hm+|mm-?hmm?|mm+)($|[\s,.!?])")
        .expect("valid filler pattern")
});

static MULTI_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]{2,}").expect("valid multi-space pattern"));

static SPACE_BEFORE_PUNCT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+([,.!?;:])").expect("valid punct pattern"));

pub fn clean_transcript(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut t = raw.trim().to_owned();
    for rule in REPLACE_RULES.iter() {
        t = rule.replace_all(&t, "").into_owned();
    }
    t = strip_fillers(&t);
    t = MULTI_SPACE.replace_all(&t, " ").into_owned();
    t = SPACE_BEFORE_PUNCT.replace_all(&t, "$1").into_owned();
    t.trim_matches([' ', ',', ';']).to_owned()
}

fn strip_fillers(t: &str) -> String {
    let mut current = t.to_owned();
    loop {
        let next = FILLER_RE
            .replace_all(&current, "${1} ${2}")
            .into_owned();
        if next == current {
            return next;
        }
        current = next;
    }
}

enum SavedClip {
    Text(String),
    Image(ImageData<'static>),
}

pub fn paste_text(text: &str, focus: Option<FocusTarget>) -> Result<usize, String> {
    let started = Instant::now();
    let chars = text.chars().count();
    log!("paste", "begin: {chars} chars: {text:?}");
    let error = |e: arboard::Error| e.to_string();
    let mut clipboard = Clipboard::new().map_err(|e| {
        let message = error(e);
        log!("paste", "clipboard open failed: {message}");
        message
    })?;
    let saved = clipboard
        .get_image()
        .ok()
        .map(SavedClip::Image)
        .or_else(|| clipboard.get_text().ok().map(SavedClip::Text));
    log!(
        "paste",
        "saved previous clipboard: {}",
        match &saved {
            Some(SavedClip::Image(_)) => "image".into(),
            Some(SavedClip::Text(previous)) => format!("text ({} chars)", previous.chars().count()),
            None => "empty".into(),
        }
    );
    clipboard.set_text(text).map_err(|e| {
        let message = error(e);
        log!("paste", "set_text failed: {message}");
        message
    })?;
    thread::sleep(PASTE_SETTLE);

    if let Some(target) = &focus {
        if let Err(error) = win_focus::restore_focus(target) {
            log!("paste", "WARN focus restore failed, keys may land on wrong window: {error}");
        }
        thread::sleep(PASTE_SETTLE);
    } else {
        log!("paste", "no captured window; sending keys to current focus");
    }

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| {
        let message = e.to_string();
        log!("paste", "enigo init failed: {message}");
        message
    })?;
    enigo.key(Key::Control, Press).map_err(|e| {
        let message = e.to_string();
        log!("paste", "ctrl press failed: {message}");
        message
    })?;
    enigo.key(Key::Unicode('v'), Click).map_err(|e| {
        let message = e.to_string();
        log!("paste", "v click failed: {message}");
        message
    })?;
    enigo.key(Key::Control, Release).map_err(|e| {
        let message = e.to_string();
        log!("paste", "ctrl release failed: {message}");
        message
    })?;
    log!("paste", "Ctrl+V sent");

    thread::sleep(RESTORE_DELAY);
    let restored = match saved {
        Some(SavedClip::Image(image)) => clipboard.set_image(image).map(|_| "image").map_err(error),
        Some(SavedClip::Text(previous)) => clipboard.set_text(previous).map(|_| "text").map_err(error),
        None => Ok("empty"),
    };
    match restored {
        Ok(kind) => log!("paste", "clipboard restored to {kind}"),
        Err(e) => log!("paste", "WARN clipboard restore failed: {e}"),
    }
    log!("paste", "done in {:.0}ms", started.elapsed().as_millis());
    Ok(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_chained_fillers_entirely() {
        assert_eq!(clean_transcript("um uh ah"), "");
        assert_eq!(clean_transcript("uh so um hello world"), "so hello world");
    }

    #[test]
    fn replacement_rules_and_case_insensitive_fillers() {
        assert_eq!(clean_transcript("mhm sure"), "sure");
        assert_eq!(clean_transcript("mm-hmm ok"), "- ok");
        assert_eq!(clean_transcript("Uh hello"), "hello");
    }

    #[test]
    fn collapses_spaces_before_punct_and_trims() {
        assert_eq!(clean_transcript("well , yes"), "well, yes");
        assert_eq!(clean_transcript("  done.  "), "done.");
        assert_eq!(clean_transcript(""), "");
    }
}
