# LWN.net Offline Archive

Personal, resumable offline archive of [LWN.net](https://lwn.net) article pages (subscriber access via cookies), plus a terminal (TUI) reader.

> Intended for personal offline reading with your own subscription. Be polite and avoid aggressive crawling.

## Repository contents

- `scrape_lwn.py` – polite, resumable archiver (HTML + assets, rewritten for offline viewing)
- `reader/` – Rust TUI reader (`Cargo.toml`, `src/main.rs`) built with `ratatui`/`crossterm`
- `.github/workflows/scrape.yml` – scheduled scraper (every 30 minutes, safe small batches)
- `.github/workflows/release.yml` – builds the reader binary and publishes a release bundle (archive + reader)
- `_meta/` – crawl state (`manifest.jsonl`, `state.json`, `failures.jsonl`)

## Scraper

```bash
python3 scrape_lwn.py --cookies /path/to/cookies.txt --out .
```

Common options:
- `--delay 5 --jitter 3` polite pacing
- `--max-pages 10` fetch at most N pages per run (recommended for scheduled runs)
- `--no-assets` save HTML only

Progress is stored in `_meta/` so runs are resumable and idempotent.

### Automated scraping (GitHub Actions)
`scrape.yml` runs every 30 minutes and commits updates back to the repo. It expects a Netscape-format `cookies.txt` in the repository root (pushed/refreshed manually when it expires) and uses conservative defaults (`--max-pages 10 --delay 5 --jitter 3`).

## TUI Reader (`lwn-reader`)

A simple terminal reader for the archive (rewritten in Rust) with:
- **Catppuccin themes** (Mocha/Latte, toggle in-app)
- **Article categories** and **search by year/category**
- **Fuzzy find** in articles (title/path/category/year)
- **Bookmarks**
- **Session restore** (reopens the last article/position when you quit while reading)
- **Initial download** of the archive on first run and **incremental updates** (fetches only new files when a new release is available, instead of redownloading the full archive)

### Install

```bash
curl -fsSL https://raw.githubusercontent.com/prjctimg/lwn-archive/main/install.sh | bash
```

Or manually: download the `lwn-reader` binary and/or the bundle tarball from the latest [release](https://github.com/prjctimg/lwn-archive/releases), extract, and place `lwn-reader` in your `PATH`.

By default the reader stores config in `~/.config/lwn-reader/` and archive data in `~/.local/share/lwn-reader/archive/`.

### Usage

```bash
lwn-reader                # start (downloads archive on first run, then checks for updates)
lwn-reader --data-dir /path/to/lwn-archive   # use an existing/local archive
lwn-reader --repo owner/repo                 # use a different archive repo
lwn-reader --theme latte|mocha
```

Environment variables: `LWN_ARCHIVE_DIR`, `LWN_ARCHIVE_REPO`, `GITHUB_TOKEN` (optional, for GitHub API rate limits).

### Key bindings

**List**
- `j/k` or arrows: move · `Enter`: open · `g/G`: top/bottom
- `/`: fuzzy find/filter · `c`: category picker · `y`: year picker
- `b`: bookmarks view · `B`: toggle bookmark for selected article
- `u`: check/download incremental update · `t`: toggle Catppuccin theme · `q`: quit

**Reader**
- `j/k` or arrows: scroll · `Space/Ctrl-F`, `b/Ctrl-B`: page down/up · `g/G`: top/end
- `B`: toggle bookmark · `q`/`Esc`: back (position is saved for session restore)

**Bookmarks**
- `Enter`: open · `d`: remove · `q`/`Esc`: back

## Building the reader from source

Requires Rust/Cargo (available on the author’s system; no additional compiler installation required):

```bash
cargo build --release --manifest-path reader/Cargo.toml
# binary: reader/target/release/lwn-reader
```

## Releases

`release.yml` builds the `lwn-reader` binary and publishes a bundle tarball containing the archive snapshot, the reader binary, `README.md`, and `install.sh`, plus `manifest.jsonl` to support incremental updates.

## Notes

- Subscriber content requires your own LWN subscription and cookies; the scheduled workflow only scrapes while valid cookies are present.
- Article pages are saved as `Articles/<id>.html` with links/assets rewritten for offline viewing. See `_meta/state.json` and `_meta/manifest.jsonl` for progress.
- This is a personal-archival tool: keep crawl rates polite.
