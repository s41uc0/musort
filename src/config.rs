use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Written by `musort init`. Kept as literal text (rather than serialized) so
/// the comments survive.
pub const DEFAULT_CONFIG: &str = r#"# musort configuration

[layout]
# Where identified tracks go, relative to the collection root.
# Fields: {artist} {album_artist} {album} {title} {year} {genre} {ext}
#         {track} {disc} (numbers; zero-pad with e.g. {track:02})
#         {disc_prefix}  "2-" on multi-disc releases, empty otherwise
# Empty brackets left behind by missing fields, like "()", are removed.
template = "{album_artist}/{album} ({year})/{disc_prefix}{track:02} - {title}.{ext}"
# Tracks that couldn't be identified (they are retried on every organize).
unidentified_dir = "_unidentified"
# Exact duplicates. musort never deletes anything; review these yourself.
duplicates_dir = "_duplicates"

[identify]
# Minimum confidence (0-100) before a lookup result is accepted and written.
min_score = 90

[identify.musicbrainz]
enabled = true
# MusicBrainz asks every client to identify itself with contact info
# (an email address or URL). Please fill this in.
contact = ""

[identify.acoustid]
# Audio fingerprinting. Needs `fpcalc` (chromaprint) on PATH and an
# application API key from https://acoustid.org/new-application
enabled = false
api_key = ""
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub layout: Layout,
    #[serde(default)]
    pub identify: Identify,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub template: String,
    pub unidentified_dir: String,
    pub duplicates_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identify {
    pub min_score: u8,
    pub musicbrainz: MusicBrainz,
    pub acoustid: AcoustId,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MusicBrainz {
    pub enabled: bool,
    pub contact: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcoustId {
    pub enabled: bool,
    pub api_key: String,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            template: "{album_artist}/{album} ({year})/{disc_prefix}{track:02} - {title}.{ext}".into(),
            unidentified_dir: "_unidentified".into(),
            duplicates_dir: "_duplicates".into(),
        }
    }
}

impl Default for Identify {
    fn default() -> Self {
        Identify {
            min_score: 90,
            musicbrainz: MusicBrainz::default(),
            acoustid: AcoustId::default(),
        }
    }
}

impl Default for MusicBrainz {
    fn default() -> Self {
        MusicBrainz { enabled: true, contact: String::new() }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("can't read {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        cfg.validate()
            .with_context(|| format!("{} is not valid", path.display()))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        crate::template::Template::parse(&self.layout.template)?;
        for (name, dir) in [
            ("unidentified_dir", &self.layout.unidentified_dir),
            ("duplicates_dir", &self.layout.duplicates_dir),
        ] {
            let p = Path::new(dir);
            if dir.is_empty()
                || p.is_absolute()
                || p.components().count() != 1
                || dir.starts_with('.')
            {
                bail!("layout.{name} must be a single plain directory name, got {dir:?}");
            }
        }
        if self.layout.unidentified_dir == self.layout.duplicates_dir {
            bail!("layout.unidentified_dir and layout.duplicates_dir must differ");
        }
        if self.identify.min_score > 100 {
            bail!("identify.min_score must be between 0 and 100");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_text_parses_and_matches_defaults() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.layout.template, Layout::default().template);
        assert!(cfg.identify.musicbrainz.enabled);
        assert!(!cfg.identify.acoustid.enabled);
    }

    #[test]
    fn rejects_bad_dirs() {
        let mut cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        cfg.layout.duplicates_dir = "../x".into();
        assert!(cfg.validate().is_err());
    }
}
