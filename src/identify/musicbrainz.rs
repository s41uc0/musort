//! MusicBrainz web service client (https://musicbrainz.org/doc/MusicBrainz_API).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::{debug, info};

use super::{hints_from_path, similarity, Match, Query, Source};
use crate::model::TrackMeta;

const BASE: &str = "https://musicbrainz.org/ws/2";

pub struct Client {
    agent: ureq::Agent,
    /// MusicBrainz allows one request per second per client.
    next_request: Instant,
    min_score: u8,
    releases: HashMap<String, Release>,
    /// Release chosen for other files in the same directory, so tracks of one
    /// album don't end up split over different editions.
    dir_release: HashMap<PathBuf, String>,
}

// ---- API types (only the parts we use) ----

#[derive(Debug, Clone, Deserialize)]
struct ArtistCredit {
    name: String,
    #[serde(default)]
    joinphrase: String,
}

fn credit(c: &[ArtistCredit]) -> Option<String> {
    let s: String = c.iter().map(|a| format!("{}{}", a.name, a.joinphrase)).collect();
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResult {
    #[serde(default)]
    recordings: Vec<Recording>,
}

#[derive(Debug, Clone, Deserialize)]
struct Recording {
    id: String,
    title: String,
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    releases: Vec<ReleaseSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseSummary {
    id: String,
    title: String,
    status: Option<String>,
    date: Option<String>,
    #[serde(rename = "release-group")]
    release_group: Option<ReleaseGroup>,
    #[serde(default)]
    media: Vec<MediumSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct MediumSummary {
    position: Option<u32>,
    #[serde(default)]
    track: Vec<TrackSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct TrackSummary {
    title: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseGroup {
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Release {
    id: String,
    title: String,
    date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "release-group")]
    release_group: Option<ReleaseGroup>,
    #[serde(default)]
    media: Vec<Medium>,
}

#[derive(Debug, Clone, Deserialize)]
struct Medium {
    position: Option<u32>,
    #[serde(rename = "track-count")]
    track_count: Option<u32>,
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
struct Track {
    position: Option<u32>,
    title: String,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    recording: TrackRecording,
}

#[derive(Debug, Clone, Deserialize)]
struct TrackRecording {
    id: String,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
}

fn year_of(date: &Option<String>) -> Option<u32> {
    date.as_deref()?.get(..4)?.parse().ok().filter(|&y| y > 0)
}

/// What we compare candidates against.
struct Wanted {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u64>,
}

impl Client {
    pub fn new(contact: &str, min_score: u8) -> Self {
        let contact = if contact.trim().is_empty() { "no contact configured" } else { contact.trim() };
        let ua = format!("musort/{} ( {contact} )", env!("CARGO_PKG_VERSION"));
        let agent = ureq::AgentBuilder::new()
            .user_agent(&ua)
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build();
        Client {
            agent,
            next_request: Instant::now(),
            min_score,
            releases: HashMap::new(),
            dir_release: HashMap::new(),
        }
    }

    fn get<T: DeserializeOwned>(&mut self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let mut attempt = 0;
        loop {
            let wait = self.next_request.saturating_duration_since(Instant::now());
            std::thread::sleep(wait);
            self.next_request = Instant::now() + Duration::from_millis(1100);

            let mut req = self.agent.get(&format!("{BASE}{path}")).query("fmt", "json");
            for (k, v) in query {
                req = req.query(k, v);
            }
            debug!("      GET {path} {query:?}");
            match req.call() {
                Ok(resp) => return resp.into_json::<T>().context("unexpected response from MusicBrainz"),
                // Rate limited / overloaded: back off and retry a few times.
                Err(ureq::Error::Status(code @ (503 | 429 | 502), _)) if attempt < 3 => {
                    attempt += 1;
                    info!("    musicbrainz is busy (HTTP {code}), retrying in {}s", 2 * attempt);
                    self.next_request = Instant::now() + Duration::from_secs(2 * attempt);
                }
                Err(ureq::Error::Status(404, _)) => bail!(NotFound),
                Err(e) => return Err(anyhow!(e).context("can't reach MusicBrainz")),
            }
        }
    }

    fn release(&mut self, id: &str) -> Result<Release> {
        if let Some(r) = self.releases.get(id) {
            debug!("    musicbrainz: release {id} (already fetched this run)");
            return Ok(r.clone());
        }
        info!("    musicbrainz: fetching the tracklist of release {id}");
        let r: Release = self.get(
            &format!("/release/{id}"),
            &[("inc", "artist-credits+recordings+release-groups")],
        )?;
        self.releases.insert(id.to_string(), r.clone());
        Ok(r)
    }

    /// Search MusicBrainz using the file's tags (and path hints for whatever
    /// tags are missing).
    pub fn search(&mut self, q: &Query<'_>) -> Result<Option<Match>> {
        let hints = hints_from_path(q.path);
        let m = q.meta;
        let Some(title) = m.title.clone().or(hints.title.clone()) else {
            info!("  musicbrainz: no title in the tags or the file name to search with");
            return Ok(None);
        };
        // A folder name is too unreliable to score releases with (it would favour
        // whichever compilation happens to resemble "my rips"), so path hints
        // only steer the search; scoring uses real tags.
        let wanted = Wanted {
            title,
            artist: m.artist.clone().or(hints.artist.clone()),
            album: m.album.clone(),
            duration_ms: (q.duration_ms > 0).then_some(q.duration_ms),
        };

        let mut base = format!("recording:\"{}\"", lucene_escape(&wanted.title));
        if let Some(a) = &wanted.artist {
            base.push_str(&format!(" AND artist:\"{}\"", lucene_escape(a)));
        }
        // Popular songs have hundreds of recordings (live versions, bootlegs),
        // and a plain search only returns some of them. So search in tiers,
        // most specific first, and stop at the first confident match:
        //   1. on the tagged album (or the folder name, but then only accept
        //      releases whose title really matches it)
        //   2. recordings on an official album, within a few seconds of the duration
        //      (no "NOT secondarytype:compilation": the index aggregates over all
        //      of a recording's releases, so that would exclude exactly the famous
        //      versions, which are on many compilations)
        //   3. anything within a few seconds of the duration
        //   4. anything
        let mut tiers: Vec<(&str, String, Option<&str>)> = Vec::new();
        if let Some(album) = &m.album {
            tiers.push(("on the tagged album", format!("{base} AND release:\"{}\"", lucene_escape(album)), None));
        } else if let Some(album) = &hints.album {
            tiers.push((
                "on an album named like the folder",
                format!("{base} AND release:\"{}\"", lucene_escape(album)),
                Some(album.as_str()),
            ));
        }
        if let Some(d) = wanted.duration_ms {
            let dur = format!(" AND dur:[{} TO {}]", d.saturating_sub(7000), d + 7000);
            tiers.push((
                "official albums, similar length",
                format!("{base}{dur} AND status:official AND primarytype:album"),
                None,
            ));
            tiers.push(("any release, similar length", format!("{base}{dur}"), None));
        }
        tiers.push(("anything", base, None));

        info!(
            "  musicbrainz: looking up {}{}{}{}",
            wanted.artist.as_deref().map(|a| format!("{a} - ")).unwrap_or_default(),
            wanted.title,
            wanted.album.as_deref().map(|a| format!(" [{a}]")).unwrap_or_default(),
            wanted.duration_ms.map(|d| format!(", {}:{:02}", d / 60_000, d / 1000 % 60)).unwrap_or_default(),
        );
        if m.title.is_none() || (m.artist.is_none() && wanted.artist.is_some()) {
            info!("    (tags are incomplete: title/artist guessed from the path, for searching only)");
        }

        let dir = q.path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let mut best: Option<(u8, Recording, ReleaseSummary)> = None;
        for (i, (label, query, require_album)) in tiers.iter().enumerate() {
            let res: SearchResult = self.get("/recording", &[("query", query), ("limit", "25")])?;
            info!("    search {}, {label}: {} recording(s)", i + 1, res.recordings.len());
            debug!("      query: {query}");
            let c = self.pick(&wanted, res.recordings, &dir, *require_album);
            match &c {
                Some((score, rec, rel)) => info!(
                    "      best: {} - {} on \"{}\"{} ({score}%{})",
                    credit(&rec.artist_credit).unwrap_or_default(),
                    rec.title,
                    rel.title,
                    year_of(&rel.date).map(|y| format!(" ({y})")).unwrap_or_default(),
                    if *score >= self.min_score { ", confident" } else { ", not confident enough" }
                ),
                None => info!("      nothing usable"),
            }
            if let Some(c) = c {
                if best.as_ref().is_none_or(|b| c.0 > b.0) {
                    best = Some(c);
                }
            }
            if best.as_ref().is_some_and(|b| b.0 >= self.min_score) {
                break;
            }
        }

        let Some((score, rec, rel)) = best else {
            info!("  musicbrainz: nothing found");
            return Ok(None);
        };
        if score < self.min_score {
            info!(
                "  musicbrainz: best candidate {} - {} [{}] scored {score}%, below min_score {}",
                credit(&rec.artist_credit).unwrap_or_default(),
                rec.title,
                rel.title,
                self.min_score
            );
            return Ok(None);
        }
        let mut found = self.complete(&rec.id, &rel.id, score, Source::MusicBrainz)?;
        if let Some(found) = &mut found {
            found.meta.ref_musicbrainz = Some(rec.id.clone());
            self.dir_release.insert(dir, rel.id.clone());
        }
        Ok(found)
    }

    /// Look up a known recording id (from AcoustID) and choose a release for it.
    pub fn by_recording(&mut self, q: &Query<'_>, recording_id: &str, score: u8) -> Result<Option<Match>> {
        info!("    musicbrainz: looking up recording {recording_id}");
        let rec: Recording = match self.get(
            &format!("/recording/{recording_id}"),
            &[("inc", "artist-credits+releases+release-groups+media")],
        ) {
            Ok(r) => r,
            Err(e) if e.is::<NotFound>() => return Ok(None),
            Err(e) => return Err(e),
        };
        let wanted = Wanted {
            title: rec.title.clone(),
            artist: credit(&rec.artist_credit),
            album: q.meta.album.clone(),
            duration_ms: None,
        };
        let dir = q.path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let Some((_, rec, rel)) = self.pick(&wanted, vec![rec], &dir, None) else {
            return Ok(None);
        };
        let found = self.complete(&rec.id, &rel.id, score, Source::AcoustId)?;
        if found.is_some() {
            self.dir_release.insert(dir, rel.id.clone());
        }
        Ok(found)
    }

    /// Score every (recording, release) pair; return the best. Among confident
    /// candidates, *which* recording and release matters more than a point or
    /// two of score. In order of preference:
    /// 1. the album the file is tagged with;
    /// 2. the release its neighbours were matched to (keeps albums together);
    /// 3. the canonical recording: one on an official studio album, and on the
    ///    most releases (live takes and bonus-disc versions are on few);
    /// 4. a release where the track is titled like the file (not "(5.1 mix)");
    /// 5. the plain official studio album over compilations, live albums and
    ///    bootlegs, disc 1 over bonus discs, then the earliest release.
    fn pick(
        &self,
        w: &Wanted,
        recs: Vec<Recording>,
        dir: &PathBuf,
        require_album: Option<&str>,
    ) -> Option<(u8, Recording, ReleaseSummary)> {
        let preferred = self.dir_release.get(dir);
        type Rank = (bool, bool, bool, bool, usize, bool, bool, bool, bool, u8, i64);
        let mut best: Option<(Rank, Recording, ReleaseSummary)> = None;
        let plain_album = |rel: &ReleaseSummary| {
            rel.status.as_deref() == Some("Official")
                && rel.release_group.as_ref().is_some_and(|g| {
                    g.primary_type.as_deref() == Some("Album") && g.secondary_types.is_empty()
                })
        };
        for rec in recs {
            let rec_score = score_recording(w, &rec);
            let canonical = rec.releases.iter().any(plain_album);
            let popularity = rec.releases.len();
            for rel in &rec.releases {
                if require_album.is_some_and(|a| similarity(a, &rel.title) < 0.8) {
                    continue;
                }
                let album_sim = w.album.as_ref().map(|a| similarity(a, &rel.title));
                let score = combine(rec_score, album_sim);
                let rank: Rank = (
                    score >= self.min_score,
                    album_sim.is_some_and(|s| s >= 0.9),
                    preferred == Some(&rel.id),
                    canonical,
                    popularity,
                    // The track title on this release ("Song (5.1 mix)", "Song (live)")
                    rel.media.iter().flat_map(|m| &m.track).all(|t| similarity(&w.title, &t.title) >= 0.9),
                    plain_album(rel),
                    rel.status.as_deref() == Some("Official"),
                    rel.media.first().and_then(|m| m.position).unwrap_or(1) == 1,
                    score,
                    -(year_of(&rel.date).unwrap_or(9999) as i64),
                );
                if best.as_ref().is_none_or(|b| rank > b.0) {
                    best = Some((rank, rec.clone(), rel.clone()));
                }
            }
        }
        best.map(|(rank, rec, rel)| (rank.9, rec, rel))
    }

    /// Fetch the release and build the full metadata for `recording_id` on it.
    fn complete(&mut self, recording_id: &str, release_id: &str, score: u8, source: Source) -> Result<Option<Match>> {
        let rel = self.release(release_id)?;
        let disc_total = rel.media.len() as u32;
        for (i, medium) in rel.media.iter().enumerate() {
            for (j, t) in medium.tracks.iter().enumerate() {
                if t.recording.id != recording_id {
                    continue;
                }
                let artist = credit(&t.artist_credit).or_else(|| credit(&t.recording.artist_credit));
                let rg = rel.release_group.as_ref();
                let meta = TrackMeta {
                    title: Some(t.title.clone()),
                    artist: artist.clone(),
                    album_artist: credit(&rel.artist_credit).or(artist),
                    album: Some(rel.title.clone()),
                    track: t.position.or(Some(j as u32 + 1)),
                    track_total: medium.track_count.or(Some(medium.tracks.len() as u32)),
                    disc: medium.position.or(Some(i as u32 + 1)),
                    disc_total: Some(disc_total.max(1)),
                    year: rg.and_then(|g| year_of(&g.first_release_date)).or_else(|| year_of(&rel.date)),
                    genre: None,
                    mb_recording_id: Some(recording_id.to_string()),
                    mb_release_id: Some(rel.id.clone()),
                    ref_musicbrainz: None,
                    ref_acoustid: None,
                };
                return Ok(Some(Match { source, score, meta }));
            }
        }
        info!("    recording {recording_id} is not on release {release_id} after all; skipping it");
        Ok(None)
    }
}

/// (sum of weighted similarities, sum of weights) over title/artist/duration.
/// Duration weighs heavily: a recording more than ~15s off is a different
/// version, however well the names match.
fn score_recording(w: &Wanted, rec: &Recording) -> (f64, f64) {
    let mut sum = 0.45 * similarity(&w.title, &rec.title);
    let mut weight = 0.45;
    if let Some(a) = &w.artist {
        sum += 0.30 * credit(&rec.artist_credit).map_or(0.0, |c| similarity(a, &c));
        weight += 0.30;
    }
    if let (Some(want), Some(got)) = (w.duration_ms, rec.length) {
        let diff = want.abs_diff(got) as f64 / 1000.0;
        let s = if diff <= 3.0 { 1.0 } else if diff >= 15.0 { 0.0 } else { 1.0 - (diff - 3.0) / 12.0 };
        sum += 0.25 * s;
        weight += 0.25;
    }
    (sum, weight)
}

fn combine((sum, weight): (f64, f64), album_sim: Option<f64>) -> u8 {
    let (sum, weight) = match album_sim {
        Some(s) => (sum + 0.15 * s, weight + 0.15),
        None => (sum, weight),
    };
    ((sum / weight) * 100.0).round().clamp(0.0, 100.0) as u8
}

fn lucene_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found on MusicBrainz")
    }
}

impl std::error::Error for NotFound {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(title: &str, artist: &str, length: Option<u64>) -> Recording {
        Recording {
            id: "r".into(),
            title: title.into(),
            length,
            artist_credit: vec![ArtistCredit { name: artist.into(), joinphrase: String::new() }],
            releases: vec![],
        }
    }

    #[test]
    fn scoring() {
        let w = Wanted {
            title: "Paranoid Android".into(),
            artist: Some("Radiohead".into()),
            album: Some("OK Computer".into()),
            duration_ms: Some(383_000),
        };
        let exact = combine(score_recording(&w, &rec("Paranoid Android", "Radiohead", Some(384_000))), Some(1.0));
        assert_eq!(exact, 100);
        let other_album = combine(score_recording(&w, &rec("Paranoid Android", "Radiohead", Some(384_000))), Some(0.2));
        assert!((85..100).contains(&other_album), "{other_album}");
        let wrong = combine(score_recording(&w, &rec("Paranoid Android", "Some Cover Band", Some(300_000))), Some(0.0));
        assert!(wrong < 60, "{wrong}");
    }

    #[test]
    fn parses_release_lookup() {
        let json = r#"{"id":"rel","title":"OK Computer","date":"1997-05-21",
            "artist-credit":[{"name":"Radiohead","joinphrase":"","artist":{"id":"a","name":"Radiohead"}}],
            "release-group":{"id":"rg","primary-type":"Album","secondary-types":[],"first-release-date":"1997-05-21"},
            "media":[{"position":1,"format":"CD","track-count":12,"tracks":[
              {"id":"t2","position":2,"number":"2","title":"Paranoid Android","length":383000,
               "recording":{"id":"rec2","title":"Paranoid Android"}}]}]}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.media[0].tracks[0].recording.id, "rec2");
        assert_eq!(year_of(&r.release_group.unwrap().first_release_date), Some(1997));
    }

    #[test]
    fn escapes_lucene() {
        assert_eq!(lucene_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }
}
