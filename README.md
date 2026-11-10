# LWN.net offline archive

Personal, resumable offline archive of LWN.net article pages (subscriber access via cookies).

## Usage

```bash
python3 scrape_lwn.py --cookies /path/to/lwn.net_cookies.txt --out /path/to/lwn-archive
```

Common options:
- `--delay 3 --jitter 1.5` polite pacing (defaults)
- `--max-pages N` fetch at most N pages this run (for testing/batching)
- `--no-assets` save HTML only (skip downloading/rewriting assets)

Progress/state is stored in `_meta/` (`manifest.jsonl`, `state.json`, `failures.jsonl`) so runs are resumable and idempotent.

## Notes
- Intended for personal offline reading with your own subscription; be polite and avoid aggressive crawling.
- Article pages are saved as `Articles/<id>.html`.
- `run_lwn_scrape.sh` (if present in parent dir) runs with a lock and appends logs to `_meta/scrape.log`.
