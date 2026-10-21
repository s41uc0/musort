//! Reading and (safely) writing tags.
//!
//! Reading uses lofty's generic view for the standard fields. Writing never
//! goes through the generic `Tag`, because converting a file's tag to it and
//! back drops anything lofty doesn't map (custom TXXX frames and so on). We
//! edit the file's own concrete tag (ID3v2, Vorbis comments, MP4 ilst) in
//! place, touching only the fields we set. Every write goes to a temp copy
//! first, is verified, and then atomically replaces the original.

use std::borrow::Cow;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::id3::v2::{
    Frame, FrameId, Id3v2Tag, Id3v2Version, TextInformationFrame, TimestampFrame, UniqueFileIdentifierFrame,
};
use lofty::tag::items::Timestamp;
use lofty::TextEncoding;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::{OpusFile, VorbisComments, VorbisFile};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, TagExt};

use crate::model::{FileInfo, TrackMeta};

/// Extensions musort treats as audio files.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp2", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "wav", "aif", "aiff",
    "aifc", "wv", "ape", "mpc",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub const REF_MUSICBRAINZ: &str = "ref:musicbrainz";
pub const REF_ACOUSTID: &str = "ref:acoustid";
const MB_UFID_OWNER: &str = "http://musicbrainz.org";
const MP4_MUSORT_MEAN: &str = "com.musort";
const MP4_ITUNES_MEAN: &str = "com.apple.iTunes";

fn parse_options() -> ParseOptions {
    // Don't bail on slightly broken tags; we never write those parts back.
    ParseOptions::new().parsing_mode(lofty::config::ParsingMode::BestAttempt)
}

/// Read everything musort needs to know about a file.
pub fn read(path: &Path) -> Result<FileInfo> {
    let tagged = (|| -> Result<lofty::file::TaggedFile> {
        Ok(Probe::open(path)?.options(parse_options()).guess_file_type()?.read()?)
    })()
    .with_context(|| format!("can't read audio file {}", path.display()))?;

    let file_type = tagged.file_type();
    let duration_ms = tagged.properties().duration().as_millis() as u64;
    let mut meta = TrackMeta::default();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        meta.title = clean(tag.title());
        meta.artist = clean(tag.artist());
        meta.album = clean(tag.album());
        meta.genre = clean(tag.genre());
        meta.album_artist = clean(tag.get_string(&ItemKey::AlbumArtist).map(Cow::Borrowed));
        meta.track = tag.track().filter(|&n| n > 0);
        meta.track_total = tag.track_total().filter(|&n| n > 0);
        meta.disc = tag.disk().filter(|&n| n > 0);
        meta.disc_total = tag.disk_total().filter(|&n| n > 0);
        meta.year = tag.year().filter(|&n| n > 0);
        meta.mb_recording_id =
            clean(tag.get_string(&ItemKey::MusicBrainzRecordingId).map(Cow::Borrowed));
        meta.mb_release_id =
            clean(tag.get_string(&ItemKey::MusicBrainzReleaseId).map(Cow::Borrowed));
    }

    let concrete = Concrete::load(path, file_type)?;
    if let Some(c) = &concrete {
        meta.ref_musicbrainz = clean(c.get(Extra::RefMusicbrainz).map(Cow::Owned));
        meta.ref_acoustid = clean(c.get(Extra::RefAcoustid).map(Cow::Owned));
        // The generic view may not surface these for every format.
        if meta.album_artist.is_none() {
            meta.album_artist = clean(c.get(Extra::AlbumArtist).map(Cow::Owned));
        }
        if meta.mb_recording_id.is_none() {
            meta.mb_recording_id = clean(c.get(Extra::MbRecordingId).map(Cow::Owned));
        }
        if meta.mb_release_id.is_none() {
            meta.mb_release_id = clean(c.get(Extra::MbReleaseId).map(Cow::Owned));
        }
    }

    Ok(FileInfo {
        format: format!("{file_type:?}"),
        duration_ms,
        writable: concrete.is_some(),
        meta,
    })
}

fn clean(s: Option<Cow<'_, str>>) -> Option<String> {
    let s = s?;
    let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!s.is_empty()).then(|| s.to_string())
}

/// Write `new` into the file at `path`. Only fields that are `Some` in `new`
/// and differ from what's in the file are touched; nothing is ever removed.
/// Returns the fields that changed.
pub fn write(path: &Path, new: &TrackMeta) -> Result<Vec<(&'static str, String, String)>> {
    let before = read(path)?;
    if !before.writable {
        bail!("musort can't safely write tags to {} files", before.format);
    }
    let mut wanted = before.meta.clone();
    overlay(&mut wanted, new);
    let changes = before.meta.diff(&wanted);
    if changes.is_empty() {
        return Ok(changes);
    }

    let file_type = file_type_of(path)?;
    let tmp = crate::fsops::tmp_path_for(path);
    let result = (|| -> Result<()> {
        std::fs::copy(path, &tmp)
            .with_context(|| format!("can't make a working copy of {}", path.display()))?;
        let mut concrete = Concrete::load(&tmp, file_type)?
            .ok_or_else(|| anyhow!("tag format became unwritable"))?;
        if concrete.is_empty() {
            // e.g. an MP3 with only an ID3v1 tag: the new ID3v2 tag becomes
            // the primary one, so carry everything over into it.
            concrete.apply(&wanted);
        } else {
            concrete.apply(&only_changed(&before.meta, &wanted));
        }
        concrete.save(&tmp)?;
        crate::fsops::fsync_file(&tmp)?;

        // Verify: the copy reads back with exactly the tags we wanted and the
        // same audio properties as the original.
        let after = read(&tmp).context("the rewritten file can't be read back")?;
        if after.meta != wanted {
            let diff = wanted.diff(&after.meta);
            bail!("tags did not read back as written: {diff:?}");
        }
        if after.duration_ms != before.duration_ms || after.format != before.format {
            bail!(
                "audio properties changed while writing tags ({} {}ms -> {} {}ms)",
                before.format,
                before.duration_ms,
                after.format,
                after.duration_ms
            );
        }
        // Atomic replace: the original is either fully old or fully new.
        std::fs::rename(&tmp, path)?;
        crate::fsops::fsync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing tags to {} failed; the file was not changed", path.display()))?;
    Ok(changes)
}

/// Copy every `Some` field of `new` over `base`.
pub fn overlay(base: &mut TrackMeta, new: &TrackMeta) {
    macro_rules! ov {
        ($($f:ident),*) => {$( if new.$f.is_some() { base.$f = new.$f.clone(); } )*};
    }
    ov!(
        title, artist, album_artist, album, track, track_total, disc, disc_total, year, genre,
        mb_recording_id, mb_release_id, ref_musicbrainz, ref_acoustid
    );
}

/// The fields of `new` that differ from `old` (others left `None`), so that
/// writing doesn't needlessly rewrite (and possibly reformat) unchanged fields.
fn only_changed(old: &TrackMeta, new: &TrackMeta) -> TrackMeta {
    let mut out = TrackMeta::default();
    macro_rules! ch {
        ($($f:ident),*) => {$( if old.$f != new.$f { out.$f = new.$f.clone(); } )*};
    }
    ch!(
        title, artist, album_artist, album, track, track_total, disc, disc_total, year, genre,
        mb_recording_id, mb_release_id, ref_musicbrainz, ref_acoustid
    );
    out
}

fn file_type_of(path: &Path) -> Result<FileType> {
    Probe::open(path)?
        .guess_file_type()?
        .file_type()
        .ok_or_else(|| anyhow!("{} is not a recognized audio file", path.display()))
}

#[derive(Clone, Copy)]
enum Extra {
    AlbumArtist,
    MbRecordingId,
    MbReleaseId,
    RefMusicbrainz,
    RefAcoustid,
}

/// A file's own tag, in the formats musort can write safely.
enum Concrete {
    Id3(Id3v2Tag),
    Vorbis(VorbisComments),
    Mp4(Ilst),
}

fn open_reader(path: &Path) -> Result<BufReader<File>> {
    Ok(BufReader::new(File::open(path)?))
}

impl Concrete {
    fn load(path: &Path, file_type: FileType) -> Result<Option<Concrete>> {
        let opts = parse_options();
        let mut r = open_reader(path)?;
        Ok(Some(match file_type {
            FileType::Mpeg => {
                let f = MpegFile::read_from(&mut r, opts)?;
                Concrete::Id3(f.id3v2().cloned().unwrap_or_default())
            }
            FileType::Flac => {
                let f = FlacFile::read_from(&mut r, opts)?;
                Concrete::Vorbis(f.vorbis_comments().cloned().unwrap_or_default())
            }
            FileType::Vorbis => {
                let f = VorbisFile::read_from(&mut r, opts)?;
                Concrete::Vorbis(f.vorbis_comments().clone())
            }
            FileType::Opus => {
                let f = OpusFile::read_from(&mut r, opts)?;
                Concrete::Vorbis(f.vorbis_comments().clone())
            }
            FileType::Mp4 => {
                let f = Mp4File::read_from(&mut r, opts)?;
                Concrete::Mp4(f.ilst().cloned().unwrap_or_default())
            }
            _ => return Ok(None),
        }))
    }

    fn is_empty(&self) -> bool {
        match self {
            Concrete::Id3(t) => t.is_empty(),
            Concrete::Vorbis(t) => t.is_empty(),
            Concrete::Mp4(t) => t.is_empty(),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        match self {
            Concrete::Id3(t) => {
                // Don't silently upgrade an ID3v2.3 file to v2.4: some players
                // still can't read 2.4.
                let v23 = t.original_version() == Id3v2Version::V3;
                t.save_to_path(path, WriteOptions::default().use_id3v23(v23))?
            }
            Concrete::Vorbis(t) => t.save_to_path(path, WriteOptions::default())?,
            Concrete::Mp4(t) => t.save_to_path(path, WriteOptions::default())?,
        }
        Ok(())
    }

    fn get(&self, which: Extra) -> Option<String> {
        match self {
            Concrete::Id3(t) => match which {
                Extra::AlbumArtist => t.get_text(&FrameId::Valid(Cow::Borrowed("TPE2"))).map(str::to_string),
                Extra::MbRecordingId => t.into_iter().find_map(|f| match f {
                    Frame::UniqueFileIdentifier(u) if u.owner == MB_UFID_OWNER => {
                        String::from_utf8(u.identifier.to_vec()).ok()
                    }
                    _ => None,
                }),
                Extra::MbReleaseId => t.get_user_text("MusicBrainz Album Id").map(str::to_string),
                Extra::RefMusicbrainz => t.get_user_text(REF_MUSICBRAINZ).map(str::to_string),
                Extra::RefAcoustid => t.get_user_text(REF_ACOUSTID).map(str::to_string),
            },
            Concrete::Vorbis(t) => vorbis_get(t, vorbis_key(which)),
            Concrete::Mp4(t) => {
                let ident = mp4_ident(which);
                t.get(&ident)?.data().find_map(|d| match d {
                    AtomData::UTF8(s) => Some(s.clone()),
                    _ => None,
                })
            }
        }
    }

    fn set(&mut self, which: Extra, value: &str) {
        let value = value.to_string();
        match self {
            Concrete::Id3(t) => match which {
                Extra::AlbumArtist => {
                    t.insert(Frame::Text(TextInformationFrame::new(
                        FrameId::Valid(Cow::Borrowed("TPE2")),
                        TextEncoding::UTF8,
                        value,
                    )));
                }
                Extra::MbRecordingId => {
                    t.insert(Frame::UniqueFileIdentifier(UniqueFileIdentifierFrame::new(
                        MB_UFID_OWNER.to_string(),
                        value.into_bytes(),
                    )));
                }
                Extra::MbReleaseId => {
                    t.insert_user_text("MusicBrainz Album Id".into(), value);
                }
                Extra::RefMusicbrainz => {
                    t.insert_user_text(REF_MUSICBRAINZ.into(), value);
                }
                Extra::RefAcoustid => {
                    t.insert_user_text(REF_ACOUSTID.into(), value);
                }
            },
            Concrete::Vorbis(t) => {
                // Remove every case variant of the key, then insert one.
                let key = vorbis_key(which);
                let existing: Vec<String> = t
                    .items()
                    .map(|(k, _)| k.to_string())
                    .filter(|k| k.eq_ignore_ascii_case(key))
                    .collect();
                for k in existing {
                    let _ = t.remove(&k).count();
                }
                t.push(key.to_string(), value);
            }
            Concrete::Mp4(t) => t.replace_atom(Atom::new(mp4_ident(which), AtomData::UTF8(value))),
        }
    }

    fn apply(&mut self, m: &TrackMeta) {
        fn acc<T: Accessor>(t: &mut T, m: &TrackMeta, set_year: bool) {
            if let Some(v) = &m.title {
                t.set_title(v.clone());
            }
            if let Some(v) = &m.artist {
                t.set_artist(v.clone());
            }
            if let Some(v) = &m.album {
                t.set_album(v.clone());
            }
            if let Some(v) = &m.genre {
                t.set_genre(v.clone());
            }
            if let Some(v) = m.track {
                t.set_track(v);
            }
            if let Some(v) = m.track_total {
                t.set_track_total(v);
            }
            if let Some(v) = m.disc {
                t.set_disk(v);
            }
            if let Some(v) = m.disc_total {
                t.set_disk_total(v);
            }
            if let Some(v) = m.year.filter(|_| set_year) {
                t.set_year(v);
            }
        }
        match self {
            Concrete::Id3(t) => {
                acc(t, m, false);
                // lofty's set_year stores TDRC as a plain text frame, which its
                // ID3v2.3 writer silently drops; a real timestamp frame converts
                // to TYER properly.
                if let Some(y) = m.year.and_then(|y| u16::try_from(y).ok()) {
                    t.insert(Frame::Timestamp(TimestampFrame::new(
                        FrameId::Valid(Cow::Borrowed("TDRC")),
                        TextEncoding::UTF8,
                        Timestamp { year: y, ..Timestamp::default() },
                    )));
                }
            }
            Concrete::Vorbis(t) => acc(t, m, true),
            Concrete::Mp4(t) => acc(t, m, true),
        }
        let extras = [
            (Extra::AlbumArtist, &m.album_artist),
            (Extra::MbRecordingId, &m.mb_recording_id),
            (Extra::MbReleaseId, &m.mb_release_id),
            (Extra::RefMusicbrainz, &m.ref_musicbrainz),
            (Extra::RefAcoustid, &m.ref_acoustid),
        ];
        for (which, value) in extras {
            if let Some(v) = value {
                self.set(which, v);
            }
        }
    }
}

fn vorbis_key(which: Extra) -> &'static str {
    match which {
        Extra::AlbumArtist => "ALBUMARTIST",
        Extra::MbRecordingId => "MUSICBRAINZ_TRACKID",
        Extra::MbReleaseId => "MUSICBRAINZ_ALBUMID",
        Extra::RefMusicbrainz => "REF:MUSICBRAINZ",
        Extra::RefAcoustid => "REF:ACOUSTID",
    }
}

fn vorbis_get(t: &VorbisComments, key: &str) -> Option<String> {
    t.items()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.to_string())
}

fn mp4_ident(which: Extra) -> AtomIdent<'static> {
    let free = |mean: &'static str, name: &'static str| AtomIdent::Freeform {
        mean: Cow::Borrowed(mean),
        name: Cow::Borrowed(name),
    };
    match which {
        Extra::AlbumArtist => AtomIdent::Fourcc(*b"aART"),
        Extra::MbRecordingId => free(MP4_ITUNES_MEAN, "MusicBrainz Track Id"),
        Extra::MbReleaseId => free(MP4_ITUNES_MEAN, "MusicBrainz Album Id"),
        Extra::RefMusicbrainz => free(MP4_MUSORT_MEAN, REF_MUSICBRAINZ),
        Extra::RefAcoustid => free(MP4_MUSORT_MEAN, REF_ACOUSTID),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Make a short audio file with ffmpeg (None if ffmpeg isn't available).
    fn make(dir: &Path, name: &str, args: &[&str]) -> Option<std::path::PathBuf> {
        let path = dir.join(name);
        let ok = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(args)
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("ffmpeg not available, skipping");
            return None;
        }
        Some(path)
    }

    fn identified() -> TrackMeta {
        TrackMeta {
            title: Some("Título Ünïcode".into()),
            artist: Some("Artist".into()),
            album_artist: Some("Album Artist".into()),
            album: Some("Album".into()),
            track: Some(3),
            track_total: Some(12),
            disc: Some(1),
            disc_total: Some(2),
            year: Some(1999),
            genre: None,
            mb_recording_id: Some("11111111-2222-3333-4444-555555555555".into()),
            mb_release_id: Some("66666666-7777-8888-9999-000000000000".into()),
            ref_musicbrainz: Some("11111111-2222-3333-4444-555555555555".into()),
            ref_acoustid: None,
        }
    }

    fn roundtrip(name: &str, args: &[&str], custom_probe: Option<(&str, &str)>) {
        let d = tempfile::tempdir().unwrap();
        let Some(p) = make(d.path(), name, args) else { return };
        let before = read(&p).unwrap();
        assert!(before.writable, "{name} should be writable");
        assert_eq!(before.meta.title.as_deref(), Some("Old Title"));

        let changes = write(&p, &identified()).unwrap();
        assert!(!changes.is_empty());
        let after = read(&p).unwrap();
        let mut want = identified();
        want.genre = before.meta.genre.clone();
        assert_eq!(after.meta, want, "{name}");
        assert_eq!(after.duration_ms, before.duration_ms);
        assert!(!crate::fsops::tmp_path_for(&p).exists());

        // Writing the same thing again changes nothing.
        assert!(write(&p, &identified()).unwrap().is_empty());

        // Tags musort doesn't know about survive.
        if let Some((tag, value)) = custom_probe {
            let out = Command::new("ffprobe")
                .args(["-loglevel", "error", "-show_entries", "format_tags", "-of", "default=nw=1"])
                .arg(&p)
                .output()
                .unwrap();
            let out = String::from_utf8_lossy(&out.stdout).to_lowercase();
            assert!(
                out.contains(&format!("{tag}={value}").to_lowercase()),
                "{name}: custom tag lost:\n{out}"
            );
        }
    }

    #[test]
    fn mp3_roundtrip() {
        roundtrip(
            "a.mp3",
            &["-metadata", "title=Old Title", "-metadata", "TXXX=custom=keepme"],
            Some(("TXXX", "custom=keepme")),
        );
    }

    #[test]
    fn mp3_id3v23_stays_v23() {
        let d = tempfile::tempdir().unwrap();
        let Some(p) = make(d.path(), "a.mp3", &["-id3v2_version", "3", "-metadata", "title=Old Title"]) else {
            return;
        };
        write(&p, &identified()).unwrap();
        let f = MpegFile::read_from(&mut open_reader(&p).unwrap(), parse_options()).unwrap();
        assert_eq!(f.id3v2().unwrap().original_version(), Id3v2Version::V3);
        assert_eq!(read(&p).unwrap().meta.ref_musicbrainz, identified().ref_musicbrainz);
    }

    #[test]
    fn flac_roundtrip() {
        roundtrip(
            "a.flac",
            &["-metadata", "title=Old Title", "-metadata", "custom_field=keepme"],
            Some(("custom_field", "keepme")),
        );
    }

    #[test]
    fn ogg_roundtrip() {
        roundtrip("a.ogg", &["-c:a", "libvorbis", "-metadata", "title=Old Title"], None);
    }

    #[test]
    fn opus_roundtrip() {
        roundtrip("a.opus", &["-c:a", "libopus", "-metadata", "title=Old Title"], None);
    }

    #[test]
    fn m4a_roundtrip() {
        roundtrip("a.m4a", &["-c:a", "aac", "-metadata", "title=Old Title"], None);
    }

    #[test]
    fn untagged_mp3_gets_a_tag() {
        let d = tempfile::tempdir().unwrap();
        let Some(p) = make(d.path(), "a.mp3", &[]) else { return };
        write(&p, &identified()).unwrap();
        assert_eq!(read(&p).unwrap().meta.title, identified().title);
    }

    #[test]
    fn wav_is_read_only() {
        let d = tempfile::tempdir().unwrap();
        let Some(p) = make(d.path(), "a.wav", &[]) else { return };
        assert!(!read(&p).unwrap().writable);
        assert!(write(&p, &identified()).is_err());
    }
}
