//! Identification: turning a file's (possibly incomplete) tags into a
//! confident MusicBrainz match.

pub mod acoustid;
pub mod musicbrainz;

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use crate::cli::ForceSource;
use crate::config::Config;
use crate::model::TrackMeta;

/// Runs the enabled (or forced) identification sources, in order.
pub struct Engine {
    mb: Option<musicbrainz::Client>,
    acoustid: Option<acoustid::Client>,
    sources: Vec<Source>,
    min_score: u8,
    net_failures: u32,
    offline: bool,
}

pub enum Outcome {
    Found(Box<Match>),
    /// Every source answered, none was confident.
    NotFound,
    /// A source couldn't be asked (network down, fpcalc missing, ...).
    Failed,
}

impl Engine {
    pub fn new(cfg: &Config, force: Option<ForceSource>, no_network: bool) -> Result<Self> {
        let id = &cfg.identify;
        let sources = match force {
            _ if no_network => vec![],
            Some(ForceSource::Musicbrainz) => vec![Source::MusicBrainz],
            Some(ForceSource::Acoustid) => vec![Source::AcoustId],
            None => [
                (id.musicbrainz.enabled, Source::MusicBrainz),
                (id.acoustid.enabled, Source::AcoustId),
            ]
            .into_iter()
            .filter_map(|(on, s)| on.then_some(s))
            .collect(),
        };
        // AcoustID only gives a recording id; release details always come
        // from MusicBrainz, so the client is needed for either source.
        let mb = (!sources.is_empty()).then(|| musicbrainz::Client::new(&id.musicbrainz.contact, id.min_score));
        let acoustid = if sources.contains(&Source::AcoustId) {
            Some(acoustid::Client::new(&id.acoustid.api_key)?)
        } else {
            None
        };
        if mb.is_some() && id.musicbrainz.contact.trim().is_empty() {
            warn!(
                "identify.musicbrainz.contact is not set in .musort/config.toml; MusicBrainz asks \
                 every client for contact info (an email address or URL)"
            );
        }
        Ok(Engine { mb, acoustid, sources, min_score: id.min_score, net_failures: 0, offline: false })
    }

    /// Whether lookups will be attempted at all.
    pub fn active(&self) -> bool {
        !self.sources.is_empty() && !self.offline
    }

    pub fn identify(&mut self, q: &Query<'_>) -> Outcome {
        if !self.active() {
            return Outcome::NotFound;
        }
        let mut failure = None;
        for source in self.sources.clone() {
            let res = match source {
                Source::MusicBrainz => self.mb.as_mut().expect("mb client").search(q),
                Source::AcoustId => self.via_acoustid(q),
            };
            match res {
                Ok(Some(m)) => {
                    self.net_failures = 0;
                    return Outcome::Found(Box::new(m));
                }
                Ok(None) => self.net_failures = 0,
                Err(e) => {
                    let network = e.chain().any(|c| c.downcast_ref::<ureq::Error>().is_some());
                    warn!(path = %q.path.display(), source = source.name(), "lookup failed: {e:#}");
                    if network {
                        self.net_failures += 1;
                        if self.net_failures >= 3 {
                            warn!("lookups keep failing to connect; continuing without network for the rest of this run");
                            self.offline = true;
                        }
                    }
                    failure = Some(e);
                }
            }
        }
        match failure {
            Some(_) => Outcome::Failed,
            None => Outcome::NotFound,
        }
    }

    fn via_acoustid(&mut self, q: &Query<'_>) -> Result<Option<Match>> {
        info!("  acoustid: fingerprinting the audio and looking it up");
        let hits = self.acoustid.as_mut().expect("acoustid client").lookup(q.path)?;
        for h in hits.iter().take(5) {
            info!("    fingerprint match {} ({}%), {} recording(s)", h.acoustid, h.score, h.recordings.len());
        }
        if hits.is_empty() {
            info!("    no fingerprint matches");
        }
        for hit in hits.iter().filter(|h| h.score >= self.min_score).take(3) {
            for rec in hit.recordings.iter().take(5) {
                let mb = self.mb.as_mut().expect("mb client");
                if let Some(mut m) = mb.by_recording(q, rec, hit.score)? {
                    m.meta.ref_acoustid = Some(hit.acoustid.clone());
                    return Ok(Some(m));
                }
            }
        }
        info!(
            "  acoustid: no confident match (best fingerprint score {}%, min_score {})",
            hits.first().map(|h| h.score).unwrap_or(0),
            self.min_score
        );
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    MusicBrainz,
    AcoustId,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::MusicBrainz => "musicbrainz",
            Source::AcoustId => "acoustid",
        }
    }
}

/// A confident identification.
#[derive(Debug, Clone)]
pub struct Match {
    pub source: Source,
    /// 0-100.
    pub score: u8,
    /// The metadata to write. Includes the `ref:` field for `source`.
    pub meta: TrackMeta,
}

/// What a lookup needs to know about a file.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub path: &'a Path,
    pub meta: &'a TrackMeta,
    pub duration_ms: u64,
}

/// Search hints from the path, for files with missing tags. These are only
/// used to *search*; they are never written to a file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hints {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub track: Option<u32>,
}

/// Guess artist/album/track/title from `.../Artist/Album/03 - Title.ext`
/// or `Artist - Title.ext`.
pub fn hints_from_path(path: &Path) -> Hints {
    let mut h = Hints::default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = stem.replace('_', " ");
    let mut rest = stem.trim();

    // Leading track number: "03 - x", "03. x", "3 x", "1-03 x"
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    let num_part = digits.trim_end_matches('-');
    if !num_part.is_empty() && num_part.len() < rest.len() {
        let n = num_part.rsplit('-').next().unwrap_or(num_part);
        if let Ok(n) = n.parse::<u32>() {
            if n > 0 && n < 1000 {
                h.track = Some(n);
                rest = rest[digits.len()..].trim_start_matches([' ', '.', '-', ')']).trim();
            }
        }
    }
    match rest.split_once(" - ") {
        Some((a, t)) => {
            h.artist = Some(a.trim().to_string());
            h.title = Some(t.trim().to_string());
        }
        None => h.title = Some(rest.to_string()).filter(|s| !s.is_empty()),
    }

    let mut parents = path.ancestors().skip(1).filter_map(|p| p.file_name()).map(|n| n.to_string_lossy());
    if let Some(album) = parents.next() {
        // Strip a trailing " (1997)" / " [1997]" from the folder name.
        let a = album.trim();
        let a = match a.rfind([' ']) {
            Some(i) if a[i + 1..].trim_matches(['(', ')', '[', ']']).parse::<u32>().is_ok() => &a[..i],
            _ => a,
        };
        h.album = Some(a.to_string());
        if h.artist.is_none() {
            h.artist = parents.next().map(|s| s.trim().to_string());
        }
    }
    h
}

/// Lowercase and drop everything but letters and digits, for comparison.
pub fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Similarity between two strings, 0.0-1.0, ignoring case and punctuation.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    strsim::normalized_levenshtein(&a, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_from_structured_path() {
        let h = hints_from_path(Path::new("/dl/Radiohead/OK Computer (1997)/02 - Paranoid Android.mp3"));
        assert_eq!(h.artist.as_deref(), Some("Radiohead"));
        assert_eq!(h.album.as_deref(), Some("OK Computer"));
        assert_eq!(h.title.as_deref(), Some("Paranoid Android"));
        assert_eq!(h.track, Some(2));
    }

    #[test]
    fn hints_from_flat_name() {
        let h = hints_from_path(Path::new("/dl/misc/Radiohead - Creep.mp3"));
        assert_eq!(h.artist.as_deref(), Some("Radiohead"));
        assert_eq!(h.title.as_deref(), Some("Creep"));
        assert_eq!(h.track, None);
    }

    #[test]
    fn similarity_ignores_case_and_punctuation() {
        assert_eq!(similarity("Don't Stop", "dont stop"), 1.0);
        assert!(similarity("Paranoid Android", "Paranoid Andriod") > 0.85);
        assert!(similarity("Creep", "Karma Police") < 0.3);
    }
}
