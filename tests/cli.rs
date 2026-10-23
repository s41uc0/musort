//! End-to-end tests of the musort binary (offline: `--no-network`).
//! Audio fixtures are generated with ffmpeg; the tests skip without it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn musort(cwd: &Path, args: &[&str]) -> Output {
    musort_env(cwd, args, &[])
}

fn musort_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_musort"))
        .current_dir(cwd)
        .args(args)
        .envs(env.iter().copied())
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
    assert!(text(&out).contains("1 other file(s)"));

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

    // Every run has its own log and history.
    let log = fs::read_to_string(run_file(&f.lib, "_import.log")).unwrap();
    assert!(log.contains("copied") && log.contains("run finished"), "{log}");
    assert!(run_file(&f.lib, "_import.jsonl").exists());
}

/// The one file in `.musort/logs/` whose name ends with `suffix`.
fn run_file(lib: &Path, suffix: &str) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(lib.join(".musort/logs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.into_iter().next().unwrap()
}

#[test]
fn undo_puts_a_move_import_back_and_can_itself_be_undone() {
    let Some(f) = fixture() else { return };
    let before = files(&f.dl);
    let out = musort(&f.lib, &["import", "../dl", "--no-network"]);
    assert!(text(&out).contains("undo: musort undo"), "{}", text(&out));
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY]);

    let out = musort(&f.lib, &["undo", "last", "--dry-run"]);
    assert!(out.status.success());
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY], "dry run changes nothing");

    let out = musort(&f.lib, &["undo", "last"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("3 file(s) moved back"), "{}", text(&out));
    assert_eq!(files(&f.dl), before);
    assert!(files(&f.lib).is_empty());
    assert!(!f.lib.join("The Testers").exists(), "emptied folders are cleaned up");

    // Undoing the undo redoes the import.
    let out = musort(&f.lib, &["undo", "last"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(files(&f.lib), vec![ONE, TWO, MYSTERY]);

    // The log lists all three runs, and the undone one says so.
    let out = musort(&f.lib, &["log"]);
    let t = text(&out);
    assert_eq!(t.matches("undo ").count(), 4, "{t}"); // 2 runs + 2 "undone by"
    assert!(t.contains("undone by"), "{t}");
}

#[test]
fn undo_removes_copies_and_leaves_changed_files_alone() {
    let Some(f) = fixture() else { return };
    let before = files(&f.dl);
    musort(&f.lib, &["import", "../dl", "--copy", "--no-network"]);
    // Someone edits one of the copies afterwards.
    let mut data = fs::read(f.lib.join(ONE)).unwrap();
    data.extend_from_slice(b"edited");
    fs::write(f.lib.join(ONE), data).unwrap();

    let out = musort(&f.lib, &["undo", "last"]);
    let t = text(&out);
    assert!(t.contains("left alone"), "{t}");
    assert!(t.contains("2 cop(ies) removed"), "{t}");
    assert_eq!(files(&f.lib), vec![ONE], "the edited copy is kept");
    assert_eq!(files(&f.dl), before, "originals untouched");
}

#[test]
fn quick_picks_up_an_interrupted_import_without_rescanning() {
    let Some(f) = fixture() else { return };
    // Interrupted (as if by Ctrl+C) after the first file.
    let out = musort_env(&f.lib, &["import", "../dl", "--no-network"], &[("MUSORT_TEST_STOP_AFTER", "1")]);
    assert_eq!(out.status.code(), Some(130), "{}", text(&out));
    assert!(text(&out).contains("with -q"), "{}", text(&out));
    assert_eq!(files(&f.lib), vec![MYSTERY]);

    // One of the remaining files disappears in the meantime.
    fs::remove_file(f.dl.join("rip/02.flac")).unwrap();

    let out = musort(&f.lib, &["import", "../dl", "-q", "--no-network"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("picking up the import") && t.contains("2 of 3 file(s) left"), "{t}");
    assert!(t.contains("1 file(s) are gone"), "{t}");
    assert!(!t.contains("found "), "no rescan: {t}");
    assert_eq!(files(&f.lib), vec![ONE, MYSTERY]);

    // Finished: nothing left to pick up, so -q scans normally.
    let out = musort(&f.lib, &["import", "../dl", "-q", "--no-network"]);
    assert!(text(&out).contains("no interrupted import"), "{}", text(&out));
}

#[test]
fn quick_organize_checks_known_files_only() {
    let Some(f) = fixture() else { return };
    musort(&f.lib, &["import", "../dl", "--copy", "--no-network"]);
    // A file removed and a file added behind musort's back.
    fs::remove_file(f.lib.join(MYSTERY)).unwrap();
    assert!(album_track(&f.lib.join("new.mp3"), 470, 3, "Three"));

    let out = musort(&f.lib, &["organize", "-q", "--no-network"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("checking the 3 file(s) musort knows about"), "{t}");
    assert!(t.contains("1 file(s) are gone"), "{t}");
    assert_eq!(files(&f.lib), vec![ONE, TWO, "new.mp3"], "new files aren't noticed by -q");

    // A full organize does notice it.
    musort(&f.lib, &["organize", "--no-network"]);
    assert_eq!(files(&f.lib), vec![ONE, TWO, "The Testers/Fixtures (2020)/03 - Three.mp3"]);
}

#[test]
fn log_shows_runs_and_file_history() {
    let Some(f) = fixture() else { return };
    musort(&f.lib, &["import", "../dl", "--no-network"]);

    let out = musort(&f.lib, &["log", "last"]);
    assert!(out.status.success());
    let t = text(&out);
    assert!(t.contains("rip/01.mp3") && t.contains(ONE), "{t}");

    let out = musort(&f.lib, &["log", ONE]);
    assert!(out.status.success());
    let t = text(&out);
    assert!(t.contains("originally") && t.contains("dl/rip/01.mp3"), "{t}");

    let out = musort(&f.lib, &["log", "no-such-run"]);
    assert!(!out.status.success());
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
fn quick_fingerprint_collisions_are_settled_by_full_hash() {
    let Some(f) = fixture() else { return };
    // Big enough (> 2 MiB) that the quick fingerprint doesn't read it all.
    let x = f.lib.join("x.mp3");
    let ok = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=300:duration=200", "-b:a", "128k"])
        .arg(&x)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let data = fs::read(&x).unwrap();
    assert!(data.len() > 3 << 20, "fixture too small: {}", data.len());
    musort(&f.lib, &["organize", "--no-network"]);
    assert_eq!(files(&f.lib), vec!["_unidentified/x.mp3"]);

    // Same size and same ends, different middle: not a duplicate.
    let incoming = f.root.join("incoming");
    fs::create_dir_all(&incoming).unwrap();
    let mut changed = data.clone();
    let mid = changed.len() / 2;
    changed[mid] ^= 0xff;
    fs::write(incoming.join("y.mp3"), changed).unwrap();
    // Byte for byte the same: a duplicate.
    fs::write(incoming.join("z.mp3"), &data).unwrap();

    let out = musort(&f.lib, &["import", "../incoming", "--no-network"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        files(&f.lib),
        vec!["_duplicates/incoming/z.mp3", "_unidentified/incoming/y.mp3", "_unidentified/x.mp3"]
    );
    let log = fs::read_to_string(run_file(&f.lib, "_import.log")).unwrap();
    assert!(log.contains("comparing full hashes"), "{log}");
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
