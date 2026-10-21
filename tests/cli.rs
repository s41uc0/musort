//! End-to-end tests of the musort binary (offline: `--no-network`).
//! Audio fixtures are generated with ffmpeg; the tests skip without it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musort(cwd: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_musort"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run musort");
    eprintln!(
        "$ musort {}\n{}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// A short tagged sine wave. `tags` are ffmpeg -metadata key=value pairs.
fn audio(path: &Path, freq: u32, tags: &[(&str, &str)]) -> bool {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={freq}:duration=1"));
    for (k, v) in tags {
        cmd.arg("-metadata").arg(format!("{k}={v}"));
    }
    cmd.arg(path).status().map(|s| s.success()).unwrap_or(false)
}

fn album_track(path: &Path, freq: u32, n: u32, title: &str) -> bool {
    let n = n.to_string();
    audio(
        path,
        freq,
        &[("artist", "The Testers"), ("album", "Fixtures"), ("date", "2020"), ("title", title), ("track", &n)],
    )
}

/// Every regular file under `dir`, relative, excluding `.musort/`.
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().unwrap() == ".musort" {
                continue;
            }
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = vec![];
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    lib: PathBuf,
    dl: PathBuf,
}

/// A collection `lib/` and a download folder `dl/` with an album, an
/// untagged file and a cover image.
fn fixture() -> Option<Fixture> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let dl = root.join("dl");
    let ok = album_track(&dl.join("rip/01.mp3"), 440, 1, "One")
        && album_track(&dl.join("rip/02.flac"), 450, 2, "Two: The Sequel")
        && audio(&dl.join("loose/mystery.mp3"), 460, &[]);
    if !ok {
        eprintln!("ffmpeg not available, skipping");
        return None;
    }
    fs::write(dl.join("rip/cover.jpg"), b"not really a jpeg").unwrap();
    let lib = root.join("lib");
    assert!(musort(&root, &["init", "lib"]).status.success());
    Some(Fixture { _tmp: tmp, root, lib, dl })
}

const ONE: &str = "The Testers/Fixtures (2020)/01 - One.mp3";
const TWO: &str = "The Testers/Fixtures (2020)/02 - Two_ The Sequel.flac";
const MYSTERY: &str = "_unidentified/dl/loose/mystery.mp3";

#[test]
fn copy_import_then_organize_is_idempotent() {
    let Some(f) = fixture() else { return };
    let before = files(&f.dl);

    let out = musort(&f.lib, &["import", "../dl", "--copy", "--no-network"]);
    assert!(out.status.success());
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY]);
    assert_eq!(files(&f.dl), before, "copy-import must not touch the source");
    assert!(text(&out).contains("1 non-audio file(s)"));

    // Nothing left to do.
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(out.status.success());
    assert!(text(&out).contains("0 moved, 3 already in place"), "{}", text(&out));

    // The database is only a cache: without it, nothing moves either.
    fs::remove_file(f.lib.join(".musort/database.db")).unwrap();
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(out.status.success());
    assert!(text(&out).contains("0 moved, 3 already in place"), "{}", text(&out));
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY]);

    // Everything is logged.
    let log = fs::read_to_string(f.lib.join(".musort/musort.log")).unwrap();
    assert!(log.contains("copied") && log.contains("run finished"));
}

#[test]
fn move_import_prunes_source_and_parks_duplicates() {
    let Some(f) = fixture() else { return };
    let out = musort(&f.lib, &["import", "../dl", "--no-network"]);
    assert!(out.status.success());
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY]);
    // Only the cover is left; the emptied `loose/` folder is gone.
    assert_eq!(files(&f.dl), vec!["rip/cover.jpg"]);
    assert!(!f.dl.join("loose").exists());
    assert!(f.dl.exists(), "the import root itself is never removed");

    // The same album again: exact duplicates are parked, never deleted.
    let again = f.root.join("again");
    assert!(album_track(&again.join("01.mp3"), 440, 1, "One"));
    let out = musort(&f.lib, &["import", "../again", "--no-network"]);
    assert!(out.status.success());
    assert!(text(&out).contains("1 duplicate(s)"), "{}", text(&out));
    assert_eq!(files(&f.lib), vec![ONE, TWO, "_duplicates/again/01.mp3", MYSTERY]);
}

#[test]
fn organize_sorts_files_dropped_into_the_collection() {
    let Some(f) = fixture() else { return };
    assert!(album_track(&f.lib.join("incoming/x.mp3"), 440, 1, "One"));
    assert!(audio(&f.lib.join("whatever.mp3"), 470, &[("title", "Only A Title")]));
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(out.status.success());
    assert_eq!(files(&f.lib), vec![ONE, "_unidentified/whatever.mp3"]);
    assert!(!f.lib.join("incoming").exists(), "emptied folders are cleaned up");

    // Fixing the tags of an unidentified file gets it sorted next time.
    let fixed = f.root.join("fixed.mp3");
    assert!(album_track(&fixed, 470, 3, "Three"));
    fs::rename(&fixed, f.lib.join("_unidentified/whatever.mp3")).unwrap();
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(out.status.success());
    assert_eq!(files(&f.lib), vec![ONE, "The Testers/Fixtures (2020)/03 - Three.mp3"]);
}

#[test]
fn same_recording_at_destination_is_a_duplicate() {
    let Some(f) = fixture() else { return };
    assert!(album_track(&f.lib.join("a.mp3"), 440, 1, "One"));
    // Different audio (so not an exact duplicate), but tagged as the same track.
    assert!(audio(
        &f.lib.join("b.mp3"),
        880,
        &[("artist", "The Testers"), ("album", "Fixtures"), ("date", "2020"), ("title", "One"), ("track", "1"), ("comment", "different")]
    ));
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(out.status.success());
    // Same artist/album/track/title: treated as the same recording.
    assert_eq!(files(&f.lib), vec![ONE, "_duplicates/b.mp3"]);
    // Stable on re-run.
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(text(&out).contains("0 moved"), "{}", text(&out));
}

#[test]
fn dry_run_changes_nothing() {
    let Some(f) = fixture() else { return };
    let before_dl = files(&f.dl);
    let out = musort(&f.lib, &["import", "../dl", "--dry-run", "--no-network"]);
    assert!(out.status.success());
    assert!(text(&out).contains("would be moved"), "{}", text(&out));
    assert!(text(&out).contains("01 - One.mp3"));
    assert_eq!(files(&f.dl), before_dl);
    assert!(files(&f.lib).is_empty());
}

#[test]
fn refuses_dangerous_requests() {
    let Some(f) = fixture() else { return };
    // Nested collection.
    let out = musort(&f.lib, &["init", "sub"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("nested"));
    // Importing from inside the collection.
    fs::create_dir_all(f.lib.join("inside")).unwrap();
    let out = musort(&f.lib, &["import", "inside", "--no-network"]);
    assert!(!out.status.success());
    // Importing a folder that contains the collection.
    let out = musort(&f.lib, &["import", "..", "--no-network"]);
    assert!(!out.status.success());
    // Commands outside any collection.
    let out = musort(&f.dl, &["organize"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("musort init"));
    // A broken config is reported, not ignored.
    fs::write(f.lib.join(".musort/config.toml"), "[layout]\ntemplate = \"{nope}\"\n").unwrap();
    let out = musort(&f.lib, &["organize", "--no-network"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("unknown field"), "{}", text(&out));
}

#[test]
fn status_reports_unidentified() {
    let Some(f) = fixture() else { return };
    musort(&f.lib, &["import", "../dl", "--copy", "--no-network"]);
    let out = musort(&f.lib, &["status"]);
    assert!(out.status.success());
    let t = text(&out);
    assert!(t.contains("2 organized") && t.contains("1 unidentified"), "{t}");
    assert!(t.contains(MYSTERY));
}
