# musort

An idiot-proof music collection organizer. It identifies your music and files
it into a consistent layout. It never overwrites or deletes a song, and it logs
everything it does.

```sh
musort init ~/Music                  # make ~/Music a collection (creates ~/Music/.musort/)
cd ~/Music
musort import ~/Downloads/new-album  # identify, tag and move new music in
musort organize                      # (re)sort everything already in the collection
musort status                        # what's identified, what isn't, recent runs
musort log                           # what every run did
musort undo last                     # changed your mind? put it all back
```

## Commands

| command | what it does |
|---|---|
| `init [PATH]` | Turn a directory into a collection. Refuses to nest collections. |
| `organize` | Identify every file in the collection that isn't identified yet, and move everything into the layout. Safe to run as often as you like. |
| `import SRC` | Identify the music in `SRC` (a folder or one file) and **move** it into the collection. Folders that end up empty are removed; covers and other non-audio files are left in place. `--copy` leaves the source untouched instead. |
| `status` | Summary from the database: counts, unidentified files, possible duplicates, recent runs. |
| `log [RUN\|FILE]` | Without arguments: the recent runs (`--all` for every run). With a run (its id, a unique prefix of it, or `last`): every file it moved, copied or retagged, with each tag change. With a file: its whole history, back to where it originally came from. |
| `undo RUN` | Revert a run (`last` = the latest run that changed something): move files back, restore their old tags exactly, remove copies an `import --copy` made. A file that changed since the run is left alone and reported. `-n` shows what would happen. An undo is a run too, so it can be undone. |

Options for `organize` and `import`:

- `-n, --dry-run`: show what would happen. No file, tag or database is touched.
- `--force musicbrainz|acoustid`: re-identify every file with that source,
  even files that already carry a `ref:` tag, and even when the source is
  disabled in the config.
- `--retry`: look up files again that were looked up before and not found.
  Without it, they are skipped until their tags change.
- `--no-network`: organize by existing tags only.
- `-q, --quick`: quick and loose. musort doesn't walk the directories; it works
  from what it already knows, and only checks that each file is still there and
  unchanged (size and mtime). Gone files are skipped and changed files are
  re-read.
  - After an interrupted run, it picks up where that run stopped.
  - `organize -q` otherwise re-checks the files in the database.
  - `import -q` with nothing to pick up scans normally.
  - Files added since the last full run aren't noticed. Run without `-q` for
    that.
- `-v` / `-vv`: narrate every step on its own line instead of the status
  bar. This covers each file, why a lookup was or wasn't done, every
  MusicBrainz/AcoustID search with its best candidate and score, the match,
  every tag change and every move. `-vv` adds the raw queries, file details,
  and a line for each file that's skipped because nothing about it changed.
  The run's log file always gets all of this, with or without `-v`.
- `--root DIR`: use this collection instead of the one found by searching
  upward from the current directory.

## How a file is identified

1. A file with a `ref:musicbrainz` or `ref:acoustid` tag was identified
   before and is trusted as-is (unless `--force`).
2. **MusicBrainz** (on by default) is searched with the file's tags. Missing
   tags are filled in from the path (`Artist/Album/03 - Title.mp3`) **for
   searching only**. The search runs in tiers (tagged album, then official
   albums near the file's duration, then broader) and stops at the first
   confident match. Among confident matches it prefers the album the file is
   tagged with, the release its neighbours matched, and the canonical studio
   recording over live takes, bonus discs and compilations.
3. **AcoustID** (off by default) fingerprints the audio with `fpcalc` when
   MusicBrainz finds nothing. It identifies files with missing or wrong tags.
4. No confident match, but the tags have artist, album, title and track
   number? The file is placed by its own tags.
5. Otherwise it goes to `_unidentified/`, keeping its original relative path.

After a match, the corrected tags are written into the file, along with
`ref:musicbrainz=<recording id>` or `ref:acoustid=<acoustid id>` and the
standard MusicBrainz ids that other tools understand. Tag writing is supported
for MP3 (ID3v2; v2.3 files stay v2.3), FLAC, Ogg Vorbis, Opus and M4A. Other
formats (WAV, AIFF, WavPack, APE, MPC) are moved but never written to; their
identification is kept in the database only.

## Layout

The default layout is:

```
{album_artist}/{album} ({year})/{disc_prefix}{track:02} - {title}.{ext}
→ Radiohead/OK Computer (1997)/02 - Paranoid Android.flac
→ Various Artists/Some Compilation (2004)/2-07 - Song.mp3     (disc 2 of 2)
```

Change it in `.musort/config.toml`. Path components are made safe for every
common filesystem, including FAT and NTFS USB drives and SMB shares:
`AC/DC` becomes `AC_DC`, `:?*"<>|` become `_`, and names are length-capped.

- Exact duplicates (byte-identical) and second copies of a recording already
  at its destination go to `_duplicates/`. Nothing is ever deleted, so review
  them yourself. musort finds exact duplicates without reading everything:
  - only files of the same size can be identical;
  - of those, only ones with the same quick fingerprint are candidates;
  - a candidate is only called a duplicate after a full hash of both files
    agrees.
- If the destination holds a *different* recording, the newcomer becomes
  `Title [2].ext`.
- A track without a year tag borrows the year its album-mates have, so one
  album isn't split over two folders.

## Safety

- **Nothing is ever overwritten.** Moves use `RENAME_NOREPLACE` (or a hard
  link where that isn't supported).
- **Moves between filesystems are verified.** The file is hashed while it's
  copied to a hidden temp file. The copy is fsynced, read back and checked
  against that hash, and only then put in place. The original is removed
  after that, never before.
- **Tag writes are atomic.** Tags are written to a copy, which is read back
  and checked: the tags must be exactly what was meant, and the audio length
  and format must be unchanged. Only then does the copy replace the original.
  If anything is off, the original is left untouched.
- **Ctrl+C never interrupts a file operation.** The first press lets the
  current file finish completely (move, tag write and database record), then
  stops. Pressing again while a file is being handled quits as soon as it's
  done. Pressing again while nothing is being written quits immediately.
  Run the same command with `-q` to pick up where it stopped without
  scanning everything again.
- **Crash recovery.** Multi-step operations are journaled. After a crash,
  power loss or `kill -9`, the next run finishes or rolls back whatever was
  in flight, and logs what it did.
- **One process at a time.** A lock stops two musort runs from working on
  the same collection at once.
- **Everything can be undone.** `musort undo <run>` reverts a run from its
  history, after checking (by quick fingerprint) that each file is still
  exactly as the run left it.

## The database is not the source of truth

`.musort/database.db` caches each file's tags, fingerprints and status so
unchanged files aren't re-read or looked up again.

Every file gets a **quick fingerprint**: a hash of its size plus its first
and last MiB, where the tags and audio frames sit. That's a fraction of
reading the whole file, and any real change to the file shows up in it.
Full content hashes are only computed when they're needed:
- two files share a size and a quick fingerprint (a possible duplicate);
- a file is copied across filesystems, where it's hashed during the copy
  anyway.

A normal run doesn't trust the cache blindly. Every file is fingerprinted
again; when the fingerprint matches, the cached tags are used, otherwise the
file is read again. So:
- files retagged by another tool are noticed, even if the tool kept the
  modification time;
- files that were moved or deleted outside musort are noticed too.

`-q` is the loose version: it trusts size and modification time, and doesn't
fingerprint at all.

What a scan learns is written to the database as it goes, by a background
writer in batches, so an interrupted scan isn't wasted.

You can delete it at any time. The next `musort organize` rebuilds it without
moving anything, and without network lookups for files that carry a `ref:`
tag.

## What you see

A normal run shows one entry per file that actually changed, and a status
bar at the bottom. The bar has progress and a running tally, plus a line for
each part of musort saying what it's doing right now:
- `file`: identifying, moving, writing and verifying tags.
- `musicbrainz`: each search step with its results, rate-limit waits,
  fetching tracklists, the match.
- `acoustid`: fingerprinting and fingerprint lookups. This line only shows
  when AcoustID is enabled.

A line that's done with the current file stays dimmed, showing its last
result.

```
⇉ dl/rip/02.flac                                  tagged · mb 97%
  → Radiohead/OK Computer (1997)/02 - Paranoid Android.flac
? dl/loose/x.mp3                                  no match
  → _unidentified/dl/loose/x.mp3

    sorting  [==========>          ] 61/120  ETA 1m @ 1.1s/item   → 40  ✎ 3  ? 2
       file   31 MB  01 - Airbag.flac: identifying
musicbrainz  Radiohead – Airbag: search 2/4: official albums, similar length · waiting 0.8s for the rate limit
   acoustid  Radiohead – Let Down: no fingerprint match
```

| symbol | letter | meaning |
|---|---|---|
| `→` | `M` | moved |
| `⇉` | `C` | copied (`import --copy`) |
| `✎` | `T` | tags written, file was already in place |
| `?` | `U` | not identified, parked in `_unidentified/` |
| `≡` | `D` | duplicate, parked in `_duplicates/` (or not copied) |
| `−` | `R` | `undo`: a copy removed (its original is intact) |
| `!` | `S` | `undo`: left alone, it changed since |
| `✗` | `E` | error; the file was left as it was |

The flags say how the file was identified: `mb 97%` (MusicBrainz match and
score; yellow below 95%), `fp 91%` (AcoustID fingerprint), `tags` (placed by
its own tags), `ref` (identified on an earlier run). Files with nothing to
do are not listed. The summary at the end gives the counts, how long it took,
the lookups made, the log file, and the command to undo the run.

Colors and symbols are used on a terminal; when output is piped, or
`NO_COLOR` is set, it's plain text with the letters instead.

## Logging

Every run that can change something gets two files in `.musort/logs/`,
named after the moment it started:

- `2026-10-23_14-02-11_import.log`: the full readable story with timestamps:
  the arguments, every file's tags, each lookup's queries, candidates and
  scores, every tag change (old to new), every move or copy (with method),
  every duplicate and unidentified decision, crash recoveries, and errors.
  It's everything `-vv` shows, and more.
- `2026-10-23_14-02-11_import.jsonl`: the history `undo` and `log` work from.
  Every move, copy and tag change is recorded with its before and after. Each
  change is written (and fsynced) *before* it happens and confirmed after, so
  even a crash can't make the history miss one.

Neither is ever deleted or rotated by musort. They're plain files, so
`grep` works. Unlike the database, the history is not a cache: keep it if
you want to be able to undo.

## Config (`.musort/config.toml`)

`musort init` writes this file with comments. The important settings:

```toml
[layout]
template = "{album_artist}/{album} ({year})/{disc_prefix}{track:02} - {title}.{ext}"
unidentified_dir = "_unidentified"
duplicates_dir = "_duplicates"

[identify]
min_score = 90          # 0-100; how confident a match must be

[identify.musicbrainz]
enabled = true
contact = ""            # please set: MusicBrainz asks clients for an email/URL

[identify.acoustid]
enabled = false
api_key = ""            # https://acoustid.org/new-application; needs fpcalc
```

Template fields: `artist`, `album_artist` (falls back to `artist`), `album`,
`title`, `genre`, `ext`, the numbers `year`, `track` and `disc` (pad with e.g.
`{track:02}`), and `disc_prefix` (`2-` on multi-disc releases, empty
otherwise). Empty brackets left by a missing field, like `()`, are dropped.

MusicBrainz allows one request per second. Identifying a large, untagged
library the first time takes a while. Later runs only look up new files.

## Building

```sh
nix build            # ./result/bin/musort, with fpcalc on its PATH
nix develop          # shell with cargo, rustc, ffmpeg (for tests), fpcalc
cargo test           # unit + end-to-end tests (end-to-end ones need ffmpeg)
```

Without Nix, `cargo build --release` needs a C compiler (for the bundled
SQLite). AcoustID needs `fpcalc` from chromaprint.
