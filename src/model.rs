use serde::{Deserialize, Serialize};

/// The metadata musort cares about, as read from (or to be written to) a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub mb_recording_id: Option<String>,
    pub mb_release_id: Option<String>,
    /// `ref:musicbrainz`: set by musort once MusicBrainz identified the file.
    pub ref_musicbrainz: Option<String>,
    /// `ref:acoustid`: set by musort once AcoustID identified the file.
    pub ref_acoustid: Option<String>,
}

/// Everything read from one audio file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub format: String,
    pub duration_ms: u64,
    /// Whether musort knows how to write tags to this format without risk.
    pub writable: bool,
    pub meta: TrackMeta,
}

impl TrackMeta {
    /// Has everything the layout needs to place the file by its own tags.
    pub fn is_complete(&self) -> bool {
        nonempty(&self.title) && nonempty(&self.artist) && nonempty(&self.album) && self.track.is_some()
    }

    pub fn is_identified(&self) -> bool {
        self.ref_musicbrainz.is_some() || self.ref_acoustid.is_some()
    }

    /// Field-by-field differences, as `(name, old, new)`.
    pub fn diff(&self, new: &TrackMeta) -> Vec<(&'static str, String, String)> {
        let mut out = Vec::new();
        macro_rules! cmp {
            ($($f:ident),*) => {$(
                if self.$f != new.$f {
                    out.push((stringify!($f), show(&self.$f), show(&new.$f)));
                }
            )*};
        }
        cmp!(
            title, artist, album_artist, album, track, track_total, disc, disc_total, year, genre,
            mb_recording_id, mb_release_id, ref_musicbrainz, ref_acoustid
        );
        out
    }

    /// Everything relevant on one line:
    /// `Artist - Title [Album (1997), track 2/12, disc 1/1]`.
    pub fn summary(&self) -> String {
        let num = |n: Option<u32>, t: Option<u32>| match (n, t) {
            (Some(n), Some(t)) => format!("{n}/{t}"),
            (Some(n), None) => n.to_string(),
            (None, _) => "?".into(),
        };
        if self.title.is_none() && self.artist.is_none() && self.album.is_none() {
            return "(no tags)".into();
        }
        let mut s = self.describe();
        let mut extra = vec![];
        if let Some(a) = &self.album {
            match self.year {
                Some(y) => extra.push(format!("{a} ({y})")),
                None => extra.push(a.clone()),
            }
        }
        if self.track.is_some() {
            extra.push(format!("track {}", num(self.track, self.track_total)));
        }
        if self.disc.is_some() {
            extra.push(format!("disc {}", num(self.disc, self.disc_total)));
        }
        if let Some(aa) = self.album_artist.as_ref().filter(|aa| Some(*aa) != self.artist.as_ref()) {
            extra.push(format!("album artist {aa}"));
        }
        if !extra.is_empty() {
            s.push_str(&format!(" [{}]", extra.join(", ")));
        }
        s
    }

    /// A one-line human description, for log lines and messages.
    pub fn describe(&self) -> String {
        format!(
            "{} - {}",
            self.artist.as_deref().unwrap_or("?"),
            self.title.as_deref().unwrap_or("?")
        )
    }
}

fn nonempty(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|s| !s.trim().is_empty())
}

fn show<T: std::fmt::Display>(v: &Option<T>) -> String {
    match v {
        Some(v) => v.to_string(),
        None => "<none>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reports_changed_fields() {
        let a = TrackMeta { title: Some("a".into()), track: Some(1), ..Default::default() };
        let b = TrackMeta { title: Some("b".into()), track: Some(1), year: Some(2001), ..Default::default() };
        let d = a.diff(&b);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], ("title", "a".into(), "b".into()));
        assert_eq!(d[1], ("year", "<none>".into(), "2001".into()));
    }
}
