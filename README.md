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
```

## Commands

| command | what it does |
|---|---|
| `init [PATH]` | Turn a directory into a collection. Refuses to nest collections. |
| `organize` | Identify every file in the collection that isn't identified yet, and move everything into the layout. Safe to run as often as you like. |
| `import SRC` | Identify the music in `SRC` (a folder or one file) and **move** it into the collection. Folders that end up empty are removed; covers and other non-audio files are left in place. `--copy` leaves the source untouched instead. |
| `status` | Summary from the database: counts, unidentified files, possible duplicates, recent runs. |

Options for `organize` and `import`:

- `-n, --dry-run`: show what would happen. No file, tag or database is touched.
- `--force musicbrainz|acoustid`: re-identify every file with that source,
  even files that already carry a `ref:` tag, and even when the source is
  disabled in the config.
- `--retry`: look up files again that were looked up before and not found.
  Without it, they are skipped until their tags change.
- `--no-network`: organize by existing tags only.
- `-v` / `-vv`: narrate every step on its own line instead of a progress
  bar. This covers each file, why a lookup was or wasn't done, every
  MusicBrainz/AcoustID search with its best candidate and score, the match,
  every tag change and every move. `-vv` adds the raw queries and file reads.
  The log file always gets all of this, with or without `-v`.
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
  them yourself.
- If the destination holds a *different* recording, the newcomer becomes
  `Title [2].ext`.
- A track without a year tag borrows the year its album-mates have, so one
  album isn't split over two folders.

## Safety

- **Nothing is ever overwritten.** Moves use `RENAME_NOREPLACE` (or a hard
  link where that isn't supported).
- **Moves between filesystems are verified.** The file is copied to a hidden
  temp file, fsynced and hash-checked, and only then put in place. The
  original is removed after that, never before.
- **Tag writes are atomic.** Tags are written to a copy, which is read back
  and checked: the tags must be exactly what was meant, and the audio length
  and format must be unchanged. Only then does the copy replace the original.
  If anything is off, the original is left untouched.
- **Ctrl+C never interrupts a file operation.** The first press lets the
  current file finish completely (move, tag write and database record), then
  stops. Pressing again while a file is being handled quits as soon as it's
  done. Pressing again while nothing is being written quits immediately.
  Run the same command again to continue where it stopped.
- **Crash recovery.** Multi-step operations are journaled. After a crash,
  power loss or `kill -9`, the next run finishes or rolls back whatever was
  in flight, and logs what it did.
- **One process at a time.** A lock stops two musort runs from working on
  the same collection at once.

## The database is not the source of truth

`.musort/database.db` caches each file's tags, hash and status so unchanged
files aren't re-read or looked up again. Every run checks it against the
disk:
- a row is only trusted if size and mtime match exactly;
- files that were moved or deleted outside musort are noticed;
- files retagged by another tool are re-read.

You can delete it at any time. The next `musort organize` rebuilds it without
moving anything, and without network lookups for files that carry a `ref:`
tag.

## Logging

`.musort/musort.log` is append-only. It records every run with its
arguments, plus, for every file:
- the tags read and each lookup's result and score;
- every tag change, old value to new;
- every move or copy (with method), every duplicate and unidentified
  decision;
- every crash recovery and error.

Warnings and errors are also shown in the terminal.

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
