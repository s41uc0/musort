//! Path templates: `{album_artist}/{album} ({year})/{track:02} - {title}.{ext}`.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::model::TrackMeta;

const FIELDS: &[&str] = &[
    "artist", "album_artist", "album", "title", "year", "genre", "ext", "track", "disc", "disc_prefix",
];
const NUMERIC: &[&str] = &["year", "track", "disc"];

/// Longest file/directory name musort produces, in bytes. Below the usual 255
/// limit so the hidden `.<name>.musort-tmp` working file still fits.
const MAX_NAME: usize = 240;

#[derive(Debug, Clone)]
enum Token {
    Lit(String),
    Field { name: String, pad: Option<usize> },
}

#[derive(Debug, Clone)]
pub struct Template {
    /// One token list per path component.
    components: Vec<Vec<Token>>,
}

impl Template {
    pub fn parse(s: &str) -> Result<Self> {
        if s.trim().is_empty() {
            bail!("the layout template is empty");
        }
        if s.starts_with('/') {
            bail!("the layout template must be relative (no leading /)");
        }
        let mut components = Vec::new();
        for part in s.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                bail!("the layout template has an empty, '.' or '..' path component: {s:?}");
            }
            components.push(parse_component(part)?);
        }
        let last = components.last().unwrap();
        if !last.iter().any(|t| matches!(t, Token::Field { name, .. } if name == "ext")) {
            bail!("the file name part of the layout template must contain {{ext}}");
        }
        if !last.iter().any(|t| matches!(t, Token::Field { name, .. } if name == "title")) {
            bail!("the file name part of the layout template must contain {{title}}, or tracks would collide");
        }
        Ok(Template { components })
    }

    /// Render a relative path for `meta`. `ext` is the file's extension.
    pub fn render(&self, meta: &TrackMeta, ext: &str) -> PathBuf {
        let n = self.components.len();
        self.components
            .iter()
            .enumerate()
            .map(|(i, toks)| {
                let raw: String = toks.iter().map(|t| render_token(t, meta, ext)).collect();
                sanitize_component(&tidy(&raw, i + 1 == n), i + 1 == n)
            })
            .collect()
    }
}

fn parse_component(s: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut inner = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => inner.push(c),
                        None => bail!("unclosed '{{' in layout template"),
                    }
                }
                if !lit.is_empty() {
                    out.push(Token::Lit(std::mem::take(&mut lit)));
                }
                let (name, pad) = match inner.split_once(':') {
                    Some((name, spec)) => {
                        if !NUMERIC.contains(&name) {
                            bail!("{{{name}}} can't be padded; only {NUMERIC:?} can");
                        }
                        let width = spec
                            .strip_prefix('0')
                            .and_then(|w| w.parse::<usize>().ok())
                            .filter(|w| (1..=9).contains(w));
                        match width {
                            Some(w) => (name.to_string(), Some(w)),
                            None => bail!("bad padding {spec:?} in {{{inner}}}; use e.g. {{{name}:02}}"),
                        }
                    }
                    None => (inner.clone(), None),
                };
                if !FIELDS.contains(&name.as_str()) {
                    bail!("unknown field {{{name}}} in layout template; known fields: {}", FIELDS.join(", "));
                }
                out.push(Token::Field { name, pad });
            }
            '}' => bail!("unmatched '}}' in layout template"),
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        out.push(Token::Lit(lit));
    }
    Ok(out)
}

fn render_token(t: &Token, m: &TrackMeta, ext: &str) -> String {
    let (name, pad) = match t {
        Token::Lit(s) => return s.clone(),
        Token::Field { name, pad } => (name.as_str(), *pad),
    };
    let num = |n: Option<u32>| match (n, pad) {
        (Some(n), Some(w)) => format!("{n:0w$}"),
        (Some(n), None) => n.to_string(),
        (None, _) => String::new(),
    };
    let text = |s: &Option<String>, fallback: &str| {
        // Field values may not contain path separators.
        s.as_deref().unwrap_or(fallback).replace(['/', '\\'], "_")
    };
    match name {
        "artist" => text(&m.artist, "Unknown Artist"),
        "album_artist" => text(
            if m.album_artist.is_some() { &m.album_artist } else { &m.artist },
            "Unknown Artist",
        ),
        "album" => text(&m.album, "Unknown Album"),
        "title" => text(&m.title, "Unknown Title"),
        "genre" => text(&m.genre, ""),
        "ext" => ext.to_ascii_lowercase(),
        "year" => num(m.year),
        "track" => num(m.track),
        "disc" => num(m.disc),
        "disc_prefix" => match (m.disc, m.disc_total) {
            (Some(d), Some(t)) if t > 1 => format!("{d}-"),
            (Some(d), None) if d > 1 => format!("{d}-"),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// Clean up what missing fields leave behind: `Album ()` -> `Album`.
fn tidy(s: &str, is_file_name: bool) -> String {
    let mut s = s.to_string();
    for empty in ["()", "[]", "( )", "[ ]"] {
        s = s.replace(empty, "");
    }
    let mut out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // A missing leading/trailing field can leave a dangling separator.
    while let Some(rest) = out.strip_prefix("- ") {
        out = rest.to_string();
    }
    while let Some(rest) = out.strip_suffix(" -") {
        out = rest.to_string();
    }
    if let Some((stem, ext)) = out.rsplit_once('.').filter(|_| is_file_name) {
        let stem = stem.trim_end().trim_end_matches(" -").trim_end();
        out = format!("{stem}.{ext}");
    }
    out
}

/// Make one path component safe on any filesystem musort might write to
/// (including FAT/NTFS USB drives and SMB shares).
pub fn sanitize_component(s: &str, is_file_name: bool) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    out = out.trim().to_string();
    // No hidden files, no trailing dots/spaces (invalid on Windows).
    out = out.trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        out = "_".into();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    if out.len() > MAX_NAME {
        out = match (is_file_name, out.rsplit_once('.')) {
            (true, Some((stem, ext))) if ext.len() < 16 => {
                format!("{}.{ext}", truncate_bytes(stem, MAX_NAME - ext.len() - 1).trim_end())
            }
            _ => truncate_bytes(&out, MAX_NAME).trim_end().to_string(),
        };
    }
    out
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "{album_artist}/{album} ({year})/{disc_prefix}{track:02} - {title}.{ext}";

    fn meta() -> TrackMeta {
        TrackMeta {
            title: Some("Paranoid Android".into()),
            artist: Some("Radiohead".into()),
            album: Some("OK Computer".into()),
            track: Some(2),
            year: Some(1997),
            ..Default::default()
        }
    }

    #[test]
    fn renders_default_layout() {
        let t = Template::parse(DEFAULT).unwrap();
        assert_eq!(
            t.render(&meta(), "FLAC"),
            PathBuf::from("Radiohead/OK Computer (1997)/02 - Paranoid Android.flac")
        );
    }

    #[test]
    fn multi_disc_prefix_and_missing_year() {
        let t = Template::parse(DEFAULT).unwrap();
        let mut m = meta();
        m.year = None;
        m.disc = Some(2);
        m.disc_total = Some(2);
        m.album_artist = Some("Various Artists".into());
        assert_eq!(
            t.render(&m, "mp3"),
            PathBuf::from("Various Artists/OK Computer/2-02 - Paranoid Android.mp3")
        );
        m.disc = Some(1);
        m.disc_total = Some(1);
        assert_eq!(t.render(&m, "mp3").file_name().unwrap(), "02 - Paranoid Android.mp3");
    }

    #[test]
    fn slashes_and_unsafe_chars_in_values() {
        let t = Template::parse(DEFAULT).unwrap();
        let mut m = meta();
        m.artist = Some("AC/DC".into());
        m.title = Some("What? \"No\": <Yes>|".into());
        m.album = Some("...Dots".into());
        let p = t.render(&m, "mp3");
        let parts: Vec<_> = p.iter().map(|c| c.to_str().unwrap().to_string()).collect();
        assert_eq!(parts, vec!["AC_DC", "Dots (1997)", "02 - What_ _No__ _Yes__.mp3"]);
    }

    #[test]
    fn long_names_are_truncated_keeping_extension() {
        let t = Template::parse(DEFAULT).unwrap();
        let mut m = meta();
        m.title = Some("é".repeat(300));
        let p = t.render(&m, "flac");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.len() <= MAX_NAME);
        assert!(name.ends_with(".flac"));
    }

    #[test]
    fn reserved_and_empty() {
        assert_eq!(sanitize_component("CON", false), "_CON");
        assert_eq!(sanitize_component("  ", false), "_");
        assert_eq!(sanitize_component("a\u{0}b\n", false), "ab");
    }

    #[test]
    fn rejects_bad_templates() {
        assert!(Template::parse("{artist}/{title}").is_err()); // no ext
        assert!(Template::parse("{artist}/{nope}.{ext}").is_err());
        assert!(Template::parse("{artist}/../{title}.{ext}").is_err());
        assert!(Template::parse("{title:2}.{ext}").is_err());
        assert!(Template::parse("{title:02}.{ext}").is_err());
        assert!(Template::parse("/{title}.{ext}").is_err());
        assert!(Template::parse("{artist/{title}.{ext}").is_err());
    }
}
