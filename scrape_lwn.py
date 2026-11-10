#!/usr/bin/env python3
"""
Scrape LWN.net articles/content for personal offline reading.

- Uses a Netscape cookie file for subscriber access.
- Discovers article/index pages from Weekly Archives and major indexes.
- Saves HTML and downloaded assets with rewritten links for offline viewing.
- Polite, resumable, and idempotent.

Default scope (as selected): core articles from weekly + section indexes, no
comments, no pre-2002 extras. Saves HTML + assets under ./lwn-archive.
"""

from __future__ import annotations

import argparse
import hashlib
import http.cookiejar
import json
import os
import posixpath
import random
import re
import sys
import time
from collections import deque
from dataclasses import dataclass, asdict
from html import unescape
from html.parser import HTMLParser
from typing import Iterable, Optional
from urllib.parse import urljoin, urlparse, urlunparse, urldefrag
from urllib.request import HTTPCookieProcessor, Request, build_opener
from urllib.error import HTTPError, URLError

BASE = "https://lwn.net"
USER_AGENT = (
    "lwn-personal-archive-scraper/1.0 (+personal offline use; polite; contact=local-user)"
)

SEED_PATHS = [
    "/Archives/",
    "/Kernel/Index/",
    "/Security/Index/",
    "/Archives/PythonIndex/",
    "/Archives/GuestIndex/",
    "/Archives/ConferenceIndex/",
    "/Archives/ConferenceByYear/",
]


def log(msg: str) -> None:
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{ts}] {msg}", flush=True)


def eprint(msg: str) -> None:
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{ts}] {msg}", file=sys.stderr, flush=True)


def ensure_dir(path: str) -> None:
    os.makedirs(path, exist_ok=True)


def sanitize_component(name: str) -> str:
    name = name.strip().replace("\x00", "")
    name = re.sub(r"[\\/:*?\"<>|]", "_", name)
    name = re.sub(r"\s+", " ", name).strip()
    return name or "_"


def is_lwn_url(url: str) -> bool:
    try:
        p = urlparse(url)
    except Exception:
        return False
    if p.scheme not in ("http", "https", ""):
        return False
    if p.netloc and p.netloc not in ("lwn.net", "www.lwn.net", "static.lwn.net"):
        return False
    return True


def normalize_url(url: str, base: str = BASE) -> Optional[str]:
    if not url:
        return None
    url = url.strip()
    if url.startswith("javascript:") or url.startswith("mailto:") or url.startswith("data:"):
        return None
    url, _frag = urldefrag(url)
    abs_url = urljoin(base, url)
    p = urlparse(abs_url)
    scheme = p.scheme or "https"
    netloc = p.netloc or "lwn.net"
    if netloc == "www.lwn.net":
        netloc = "lwn.net"
    # drop common tracking params only; keep functional ones
    path = p.path or "/"
    # collapse multiple slashes
    path = re.sub(r"/{2,}", "/", path)
    query = p.query
    # normalize /Archives/ pagination n/offset kept
    return urlunparse((scheme, netloc, path, "", query, ""))


def url_to_local_path(root: str, url: str) -> str:
    p = urlparse(url)
    host = p.netloc or "lwn.net"
    path = p.path or "/"

    # Special-case article pages: /Articles/<id>/ -> Articles/<id>.html
    m_article = re.fullmatch(r"/Articles/(\d+)/?", path)
    if m_article and host in ("lwn.net", "www.lwn.net", ""):
        rel = f"Articles/{m_article.group(1)}.html"
    else:
        if not path or path.endswith("/"):
            path = (path or "/") + "index.html"
        # if path has no extension and not ending with slash originally, treat as page
        _, ext = posixpath.splitext(path)
        if not ext:
            path = path.rstrip("/") + "/index.html"
        # include query in filename if present to avoid collisions (e.g. Archives offsets)
        if p.query:
            qhash = hashlib.sha1(p.query.encode("utf-8")).hexdigest()[:10]
            base, ext2 = posixpath.splitext(path)
            safe_q = re.sub(r"[^a-zA-Z0-9._=-]", "_", p.query)[:80]
            path = f"{base}__q_{safe_q}_{qhash}{ext2 or '.html'}"
        rel = path.lstrip("/")

    # for static.lwn.net keep host folder
    if host == "static.lwn.net":
        rel = posixpath.join("_static", rel)
    local = os.path.join(root, *rel.split("/"))
    # ensure within root
    local_abs = os.path.abspath(local)
    root_abs = os.path.abspath(root)
    if not (local_abs == root_abs or local_abs.startswith(root_abs + os.sep)):
        # fallback safe path
        h = hashlib.sha1(url.encode("utf-8")).hexdigest()
        local_abs = os.path.join(root_abs, "_unsafe", h + ".bin")
    return local_abs


def local_relpath(from_file: str, to_file: str) -> str:
    rel = os.path.relpath(to_file, os.path.dirname(from_file))
    return rel.replace(os.sep, "/")


class LinkExtractor(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[tuple[str, str]] = []  # (tag_attr, url)
        self.base_href: Optional[str] = None
        self.title_parts: list[str] = []
        self._in_title = False

    def handle_starttag(self, tag: str, attrs):
        attrd = dict(attrs)
        if tag == "base" and attrd.get("href"):
            self.base_href = attrd.get("href")
        if tag == "title":
            self._in_title = True
        for attr in ("href", "src", "data-src", "srcset"):
            val = attrd.get(attr)
            if not val:
                continue
            if attr == "srcset":
                # take URLs from srcset entries
                for part in val.split(","):
                    u = part.strip().split()[0] if part.strip() else ""
                    if u:
                        self.links.append((f"{tag}:{attr}", u))
            else:
                self.links.append((f"{tag}:{attr}", val))

    def handle_endtag(self, tag: str):
        if tag == "title":
            self._in_title = False

    def handle_data(self, data: str):
        if self._in_title:
            self.title_parts.append(data)

    @property
    def title(self) -> str:
        return unescape("".join(self.title_parts)).strip()


ATTR_RE = re.compile(r"(?P<attr>\b(?:href|src|data-src)\s*=\s*)(?P<q>[\"\'])(?P<url>.*?)(?P=q)", re.IGNORECASE | re.DOTALL)
SRCSET_RE = re.compile(r"(?P<attr>\bsrcset\s*=\s*)(?P<q>[\"\'])(?P<val>.*?)(?P=q)", re.IGNORECASE | re.DOTALL)
CSS_URL_RE = re.compile(r"url\(\s*(?P<q>[\"\']?)(?P<url>[^\"\')]+)(?P=q)\s*\)", re.IGNORECASE)


@dataclass
class FetchResult:
    url: str
    final_url: str
    status: int
    content_type: str
    local_path: Optional[str]
    error: Optional[str] = None
    title: Optional[str] = None


class LWNArchiver:
    def __init__(self, args: argparse.Namespace):
        self.root = os.path.abspath(args.out)
        self.cookie_path = args.cookies
        self.delay = args.delay
        self.jitter = args.jitter
        self.timeout = args.timeout
        self.max_retries = args.max_retries
        self.assets_enabled = not args.no_assets
        self.max_pages = args.max_pages
        self.user_agent = args.user_agent
        self.include_epub = args.include_epub
        self.include_bigpage = args.include_bigpage

        self.meta_dir = os.path.join(self.root, "_meta")
        self.manifest_path = os.path.join(self.meta_dir, "manifest.jsonl")
        self.state_path = os.path.join(self.meta_dir, "state.json")
        self.fail_path = os.path.join(self.meta_dir, "failures.jsonl")
        ensure_dir(self.root)
        ensure_dir(self.meta_dir)

        self.manifest: dict[str, dict] = {}
        self._load_manifest()

        cj = http.cookiejar.MozillaCookieJar(self.cookie_path)
        cj.load(ignore_discard=True, ignore_expires=True)
        self.opener = build_opener(HTTPCookieProcessor(cj))

        self.session_ok: Optional[bool] = None

    def _load_manifest(self) -> None:
        if not os.path.exists(self.manifest_path):
            return
        with open(self.manifest_path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                    u = obj.get("url")
                    if u:
                        self.manifest[u] = obj
                except Exception:
                    continue

    def _append_manifest(self, obj: dict) -> None:
        self.manifest[obj["url"]] = obj
        with open(self.manifest_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(obj, ensure_ascii=False) + "\n")

    def _record_failure(self, url: str, err: str) -> None:
        rec = {"url": url, "error": err, "ts": time.time()}
        with open(self.fail_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    def _headers(self):
        return {
            "User-Agent": self.user_agent,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": "en-US,en;q=0.9",
            "Connection": "close",
        }

    def _sleep_polite(self, multiplier: float = 1.0) -> None:
        base = max(0.0, self.delay * multiplier)
        if base <= 0:
            return
        jitter = random.uniform(0, self.jitter) if self.jitter > 0 else 0.0
        time.sleep(base + jitter)

    def fetch_bytes(self, url: str, referer: Optional[str] = None) -> tuple[Optional[bytes], str, str, int, Optional[str]]:
        headers = self._headers()
        if referer:
            headers["Referer"] = referer
        req = Request(url, headers=headers)
        last_err = None
        for attempt in range(1, self.max_retries + 1):
            try:
                with self.opener.open(req, timeout=self.timeout) as resp:
                    data = resp.read()
                    final_url = resp.geturl()
                    ctype = resp.headers.get("Content-Type", "") or ""
                    status = getattr(resp, "status", 200)
                    return data, final_url, ctype, status, None
            except HTTPError as e:
                status = e.code
                err_body = ""
                try:
                    err_body = e.read(300).decode("utf-8", "ignore")
                except Exception:
                    pass
                last_err = f"HTTP {status} {e.reason}"
                # rate limit / server errors -> backoff
                if status in (429, 500, 502, 503, 504):
                    wait = min(300, (2 ** attempt) * self.delay + random.uniform(0, self.jitter + 1))
                    eprint(f"  retryable {status} for {url} (attempt {attempt}/{self.max_retries}), sleeping {wait:.1f}s")
                    time.sleep(wait)
                    continue
                # 403/404/410 likely permanent for this resource
                return None, url, "", status, last_err + (" | " + err_body if err_body else "")
            except URLError as e:
                last_err = f"URLError: {e}"
                wait = min(120, (2 ** attempt) * self.delay + random.uniform(0, self.jitter + 1))
                eprint(f"  network error for {url} (attempt {attempt}/{self.max_retries}): {e}; sleeping {wait:.1f}s")
                time.sleep(wait)
            except Exception as e:
                last_err = f"Error: {e}"
                wait = min(120, (2 ** attempt) * self.delay + random.uniform(0, self.jitter + 1))
                eprint(f"  error for {url} (attempt {attempt}/{self.max_retries}): {e}; sleeping {wait:.1f}s")
                time.sleep(wait)
        return None, url, "", 0, last_err

    def already_saved_ok(self, url: str) -> bool:
        rec = self.manifest.get(url)
        if not rec:
            return False
        if rec.get("status") != "ok":
            return False
        lp = rec.get("local_path")
        if not lp:
            return False
        return os.path.exists(lp) and os.path.getsize(lp) > 0

    def save_asset(self, url: str, referer: Optional[str]) -> Optional[str]:
        if not url or not is_lwn_url(url):
            return None
        url = normalize_url(url, referer or BASE)
        if not url:
            return None
        p = urlparse(url)
        # only fetch assets from lwn/static.lwn
        if p.netloc not in ("lwn.net", "static.lwn.net"):
            return None
        local_path = url_to_local_path(self.root, url)
        if os.path.exists(local_path) and os.path.getsize(local_path) > 0:
            return local_path
        ensure_dir(os.path.dirname(local_path))
        self._sleep_polite(0.5)
        data, final_url, ctype, status, err = self.fetch_bytes(url, referer=referer)
        if data is None:
            self._append_manifest({"url": url, "final_url": final_url, "status": "error", "error": err, "kind": "asset"})
            return None
        # write bytes
        tmp = local_path + ".tmp"
        with open(tmp, "wb") as f:
            f.write(data)
        os.replace(tmp, local_path)
        self._append_manifest({"url": url, "final_url": final_url, "status": "ok", "local_path": local_path, "content_type": ctype, "kind": "asset"})
        log(f"  asset: {url} -> {os.path.relpath(local_path, self.root)}")
        return local_path

    def rewrite_css(self, css_text: str, css_url: str, css_local_path: str) -> str:
        def repl(m):
            raw = m.group("url").strip()
            if raw.startswith("data:") or raw.startswith("javascript:"):
                return m.group(0)
            abs_u = normalize_url(raw, css_url)
            if not abs_u or not is_lwn_url(abs_u):
                return m.group(0)
            asset_local = self.save_asset(abs_u, referer=css_url)
            if not asset_local:
                return m.group(0)
            rel = local_relpath(css_local_path, asset_local)
            q = m.group("q") or '"'
            return f"url({q}{rel}{q})"
        return CSS_URL_RE.sub(repl, css_text)

    def rewrite_html(self, html: str, page_url: str, page_local_path: str) -> tuple[str, list[str], Optional[str]]:
        extractor = LinkExtractor()
        try:
            extractor.feed(html)
        except Exception:
            pass
        page_base = extractor.base_href and normalize_url(extractor.base_href, page_url) or page_url

        discovered: list[str] = []

        def resolve_and_maybe_asset(raw: str, tag_attr: str) -> Optional[str]:
            if not raw:
                return None
            raw = raw.strip()
            if raw.startswith("data:") or raw.startswith("javascript:") or raw.startswith("mailto:") or raw.startswith("#"):
                return None
            abs_u = normalize_url(raw, page_base)
            if not abs_u:
                return None
            # discover navigable/content links from anchors
            if tag_attr.startswith("a:"):
                discovered.append(abs_u)
            # assets to localize
            if self.assets_enabled:
                p = urlparse(abs_u)
                is_asset_tag = tag_attr.startswith(("img:", "link:", "script:", "source:", "video:", "audio:"))
                looks_asset = p.netloc == "static.lwn.net" or any(p.path.lower().endswith(ext) for ext in (".css", ".js", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".woff", ".woff2", ".ttf", ".eot"))
                if is_asset_tag or looks_asset:
                    if p.netloc in ("lwn.net", "static.lwn.net"):
                        asset_local = self.save_asset(abs_u, referer=page_url)
                        if asset_local:
                            # if css, rewrite inside it
                            if p.path.lower().endswith(".css"):
                                try:
                                    with open(asset_local, "r", encoding="utf-8", errors="ignore") as f:
                                        css = f.read()
                                    new_css = self.rewrite_css(css, abs_u, asset_local)
                                    if new_css != css:
                                        with open(asset_local, "w", encoding="utf-8") as f:
                                            f.write(new_css)
                                except Exception:
                                    pass
                            return local_relpath(page_local_path, asset_local)
            # for internal html links, if target will be/may be saved locally, we can rewrite later? better keep absolute for now except assets
            # but to improve offline nav, rewrite lwn.net links to local relative if file exists or predictable
            if is_lwn_url(abs_u):
                target_local = url_to_local_path(self.root, abs_u)
                if os.path.exists(target_local):
                    return local_relpath(page_local_path, target_local)
            return None

        def attr_repl(m):
            raw = m.group("url")
            # find which attr by searching backwards a bit not trivial; infer by tag not known; use generic
            # We'll decide asset-ness by URL/ext
            abs_u = normalize_url(raw, page_base)
            new = None
            if abs_u:
                p = urlparse(abs_u)
                tag_attr_guess = "a:href"
                if p.netloc == "static.lwn.net" or any(p.path.lower().endswith(ext) for ext in (".css", ".js", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".woff", ".woff2", ".ttf", ".eot")):
                    tag_attr_guess = "asset:href"
                new = resolve_and_maybe_asset(raw, tag_attr_guess)
            if new:
                return f"{m.group('attr')}{m.group('q')}{new}{m.group('q')}"
            # still record discovery for anchors-like html pages
            if abs_u and is_lwn_url(abs_u):
                discovered.append(abs_u)
            return m.group(0)

        def srcset_repl(m):
            val = m.group("val")
            parts = []
            changed = False
            for part in val.split(","):
                part = part.strip()
                if not part:
                    continue
                segs = part.split()
                u = segs[0]
                abs_u = normalize_url(u, page_base)
                newu = None
                if abs_u and is_lwn_url(abs_u):
                    p = urlparse(abs_u)
                    if p.netloc == "static.lwn.net" or any(p.path.lower().endswith(ext) for ext in (".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp")):
                        loc = self.save_asset(abs_u, referer=page_url)
                        if loc:
                            newu = local_relpath(page_local_path, loc)
                if newu:
                    segs[0] = newu
                    changed = True
                parts.append(" ".join(segs))
            if changed:
                return f"{m.group('attr')}{m.group('q')}{', '.join(parts)}{m.group('q')}"
            # record
            for part in val.split(","):
                u = part.strip().split()[0] if part.strip() else ""
                abs_u = normalize_url(u, page_base)
                if abs_u and is_lwn_url(abs_u):
                    discovered.append(abs_u)
            return m.group(0)

        new_html = ATTR_RE.sub(attr_repl, html)
        new_html = SRCSET_RE.sub(srcset_repl, new_html)
        # de-dup discovered preserving order
        seen = set()
        uniq = []
        for u in discovered:
            nu = normalize_url(u, page_url)
            if not nu:
                continue
            if nu not in seen:
                seen.add(nu)
                uniq.append(nu)
        return new_html, uniq, (extractor.title or None)

    def should_follow_discovered(self, url: str) -> bool:
        p = urlparse(url)
        if p.netloc not in ("lwn.net", ""):
            return False
        path = p.path
        # exclude obvious non-content/actions
        if path.startswith(("/Login", "/logout", "/subscribe", "/SubscriberLink", "/MyAccount", "/Preferences")):
            return False
        if path.startswith("/Articles/") and path.rstrip("/").split("/")[-1] == "epub" and not self.include_epub:
            return False
        if path.startswith("/Articles/") and path.rstrip("/").endswith("/bigpage") and not self.include_bigpage:
            return False
        # skip mailto-like encoded, images etc
        if re.search(r"\.(?:png|jpe?g|gif|svg|webp|css|js|ico|woff2?|ttf|eot|pdf|mp3|mp4|zip|gz|tgz)(?:$|\?)", path, re.IGNORECASE):
            return False
        # allow articles, archives, indexes, kernel/security pages, current, etc.
        allowed_prefixes = (
            "/Articles/",
            "/Archives",
            "/Kernel/",
            "/Security/",
            "/PythonIndex",
            "/Calendar",
            "/current",
            "/op/",
        )
        return path.startswith(allowed_prefixes)

    def save_page(self, url: str, kind: str = "page", referer: Optional[str] = None) -> tuple[Optional[str], list[str], Optional[str]]:
        url = normalize_url(url) or url
        if self.already_saved_ok(url):
            # still parse saved file for discovery if needed? manifest doesn't store links; rescan from disk
            rec = self.manifest[url]
            lp = rec.get("local_path")
            links = []
            title = rec.get("title")
            if lp and os.path.exists(lp) and lp.endswith((".html", ".htm")):
                try:
                    with open(lp, "r", encoding="utf-8", errors="ignore") as f:
                        html = f.read()
                    ex = LinkExtractor()
                    ex.feed(html)
                    base = ex.base_href and normalize_url(ex.base_href, url) or url
                    for _, u in ex.links:
                        au = normalize_url(u, base)
                        if au:
                            links.append(au)
                    title = title or ex.title or None
                except Exception:
                    pass
            return lp, links, title

        local_path = url_to_local_path(self.root, url)
        ensure_dir(os.path.dirname(local_path))
        log(f"GET {url}")
        self._sleep_polite()
        data, final_url, ctype, status, err = self.fetch_bytes(url, referer=referer)
        if data is None:
            eprint(f"  FAIL {url}: {err}")
            self._append_manifest({"url": url, "final_url": final_url, "status": "error", "error": err, "kind": kind})
            self._record_failure(url, err or "unknown")
            return None, [], None

        # decode if text
        is_html = "text/html" in ctype or local_path.endswith(".html") or local_path.endswith(".htm")
        discovered: list[str] = []
        title = None
        if is_html:
            text = data.decode("utf-8", errors="replace")
            rewritten, discovered, title = self.rewrite_html(text, final_url or url, local_path)
            out_bytes = rewritten.encode("utf-8")
        else:
            out_bytes = data

        tmp = local_path + ".tmp"
        with open(tmp, "wb") as f:
            f.write(out_bytes)
        os.replace(tmp, local_path)

        rec = {
            "url": url,
            "final_url": final_url,
            "status": "ok",
            "status_code": status,
            "local_path": local_path,
            "content_type": ctype,
            "kind": kind,
            "title": title,
            "ts": time.time(),
        }
        self._append_manifest(rec)
        rel = os.path.relpath(local_path, self.root)
        log(f"  saved: {rel}" + (f"  [{title}]" if title else ""))
        return local_path, discovered, title

    def crawl(self) -> None:
        queue: deque[tuple[str, str, Optional[str]]] = deque()  # url, kind, referer
        enqueued = set()

        def enqueue(u: str, kind: str, referer: Optional[str] = None):
            nu = normalize_url(u)
            if not nu:
                return
            if nu in enqueued:
                return
            if not is_lwn_url(nu):
                return
            # always allow seeds; otherwise filter
            if kind == "seed" or self.should_follow_discovered(nu):
                enqueued.add(nu)
                queue.append((nu, kind, referer))

        for sp in SEED_PATHS:
            enqueue(urljoin(BASE, sp), "seed")

        pages_processed = 0
        article_ids_seen = set()

        while queue:
            if self.max_pages and pages_processed >= self.max_pages:
                log(f"Reached --max-pages={self.max_pages}, stopping.")
                break
            url, kind, referer = queue.popleft()

            # track article id
            m = re.match(r"^https://lwn\.net/Articles/(\d+)/", url)
            if m:
                article_ids_seen.add(m.group(1))

            local_path, discovered, _title = self.save_page(url, kind=kind, referer=referer)
            pages_processed += 1

            # expand discovered links
            for d in discovered:
                nd = normalize_url(d)
                if not nd or not is_lwn_url(nd):
                    continue
                # prioritize articles and archive pagination
                if nd.startswith("https://lwn.net/Articles/"):
                    enqueue(nd, "article", referer=url)
                elif nd.startswith("https://lwn.net/Archives") or nd.startswith("https://lwn.net/Kernel/") or nd.startswith("https://lwn.net/Security/") or nd.startswith("https://lwn.net/PythonIndex") or "/Conference" in nd or nd.startswith("https://lwn.net/GuestIndex"):
                    enqueue(nd, "index", referer=url)
                elif nd.startswith("https://lwn.net/"):
                    enqueue(nd, "page", referer=url)

            # periodic progress save
            if pages_processed % 25 == 0:
                state = {
                    "pages_processed": pages_processed,
                    "queue_len": len(queue),
                    "unique_articles_seen": len(article_ids_seen),
                    "manifest_entries": len(self.manifest),
                    "ts": time.time(),
                }
                with open(self.state_path, "w", encoding="utf-8") as f:
                    json.dump(state, f, indent=2)
                log(f"Progress: processed={pages_processed} queued={len(queue)} articles_seen={len(article_ids_seen)} manifest={len(self.manifest)}")

        state = {
            "pages_processed": pages_processed,
            "queue_len": len(queue),
            "unique_articles_seen": len(article_ids_seen),
            "manifest_entries": len(self.manifest),
            "completed": len(queue) == 0,
            "ts": time.time(),
        }
        with open(self.state_path, "w", encoding="utf-8") as f:
            json.dump(state, f, indent=2)
        log(f"Done. processed={pages_processed} articles_seen={len(article_ids_seen)} manifest={len(self.manifest)}")


def parse_args(argv=None):
    ap = argparse.ArgumentParser(description="Polite, resumable LWN.net offline archiver")
    ap.add_argument("--cookies", default="/home/prjctimg/lwn.net_cookies.txt", help="Path to Netscape cookies.txt")
    ap.add_argument("--out", default="/home/prjctimg/lwn-archive", help="Output directory root")
    ap.add_argument("--delay", type=float, default=3.0, help="Base delay between page requests (seconds)")
    ap.add_argument("--jitter", type=float, default=1.5, help="Random extra delay [0,jitter] seconds")
    ap.add_argument("--timeout", type=float, default=30.0, help="HTTP timeout seconds")
    ap.add_argument("--max-retries", type=int, default=5, help="Retries for transient failures")
    ap.add_argument("--max-pages", type=int, default=0, help="Stop after N page fetches (0 = unlimited)")
    ap.add_argument("--no-assets", action="store_true", help="Do not download/rewrite assets (HTML only)")
    ap.add_argument("--include-bigpage", action="store_true", help="Also follow /Articles/.../bigpage links")
    ap.add_argument("--include-epub", action="store_true", help="Also follow /Articles/.../epub links")
    ap.add_argument("--user-agent", default=USER_AGENT, help="HTTP User-Agent")
    return ap.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    if args.max_pages <= 0:
        args.max_pages = 0
    if not os.path.exists(args.cookies):
        eprint(f"Cookie file not found: {args.cookies}")
        return 2
    log(f"Output: {os.path.abspath(args.out)}")
    log(f"Cookies: {args.cookies}")
    log(f"Assets: {'on' if not args.no_assets else 'off'}; delay={args.delay}s jitter={args.jitter}s")
    archiver = LWNArchiver(args)
    archiver.crawl()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
