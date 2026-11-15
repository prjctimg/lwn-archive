use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use directories::ProjectDirs;
use flate2::read::GzDecoder;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeName { Mocha, Latte }
#[derive(Clone)]
struct Theme {
    base: Color, mantle: Color, surface0: Color, surface1: Color,
    text: Color, subtext: Color, blue: Color, lavender: Color, mauve: Color, peach: Color, green: Color,
}
fn hex(c: u32) -> Color { Color::Rgb(((c>>16)&0xff) as u8, ((c>>8)&0xff) as u8, (c&0xff) as u8) }
fn theme(n: ThemeName) -> Theme {
    match n {
        ThemeName::Mocha => Theme { base: hex(0x1e1e2e), mantle: hex(0x181825), surface0: hex(0x313244), surface1: hex(0x45475a), text: hex(0xcdd6f4), subtext: hex(0xa6adc8), blue: hex(0x89b4fa), lavender: hex(0xb4befe), mauve: hex(0xcba6f7), peach: hex(0xfab387), green: hex(0xa6e3a1) },
        ThemeName::Latte => Theme { base: hex(0xeff1f5), mantle: hex(0xe6e9ef), surface0: hex(0xccd0da), surface1: hex(0xbcc0cc), text: hex(0x4c4f69), subtext: hex(0x6c6f85), blue: hex(0x1e66f5), lavender: hex(0x7287fd), mauve: hex(0x8839ef), peach: hex(0xfe640b), green: hex(0x40a02b) },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    repo: String,
    data_dir: String,
    theme: String,
    installed_tag: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        let data_dir = ProjectDirs::from("", "", "lwn-reader")
            .map(|p| p.data_dir().join("archive").to_string_lossy().to_string())
            .unwrap_or_else(|| "./lwn-archive-data".into());
        Self { repo: "prjctimg/lwn-archive".into(), data_dir, theme: "mocha".into(), installed_tag: None }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct AppState {
    last_view: String, // list|reader
    last_article_path: Option<String>,
    last_list_index: usize,
    last_filter: String,
    last_category: String,
    last_year: String,
    reader_offset: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bookmark { path: String, title: String, added_at: String }

#[derive(Debug, Clone)]
struct Article {
    path: String,
    title: String,
    category: String,
    year: String,
}

fn config_dir() -> PathBuf {
    ProjectDirs::from("", "", "lwn-reader").map(|p| p.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".lwn-reader"))
}
fn load_json<T: for<'d> Deserialize<'d> + Default>(p: &Path) -> T {
    fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    if let Some(parent) = p.parent() { fs::create_dir_all(parent)?; }
    fs::write(p, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let mut req = ureq::get(url).set("User-Agent", "lwn-reader");
    if let Ok(tok) = std::env::var("GITHUB_TOKEN") { req = req.set("Authorization", &format!("Bearer {}", tok)); }
    let resp = req.call().map_err(|e| anyhow!("GET {}: {}", url, e))?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}
fn http_get_string(url: &str) -> Result<String> { Ok(String::from_utf8(http_get(url)?)?) }
fn gh_api(path: &str) -> Result<serde_json::Value> {
    let url = format!("https://api.github.com{}", path);
    let mut req = ureq::get(&url).set("User-Agent", "lwn-reader").set("Accept", "application/vnd.github+json");
    if let Ok(tok) = std::env::var("GITHUB_TOKEN") { req = req.set("Authorization", &format!("Bearer {}", tok)); }
    let resp = req.call().map_err(|e| anyhow!("API {}: {}", path, e))?;
    Ok(resp.into_json()?)
}

fn latest_release(repo: &str) -> Result<serde_json::Value> { gh_api(&format!("/repos/{}/releases/latest", repo)) }

fn extract_tar_gz(data: &[u8], dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let gz = GzDecoder::new(data);
    let mut ar = tar::Archive::new(gz);
    // extract, stripping first component if present
    let entries = ar.entries()?;
    for e in entries {
        let mut e = e?;
        let p: PathBuf = e.path()?.into_owned();
        let comps: Vec<_> = p.components().collect();
        let rel: PathBuf = if comps.len() > 1 { comps[1..].iter().collect() } else { p.clone() };
        if rel.as_os_str().is_empty() { continue; }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() { fs::create_dir_all(parent)?; }
        e.unpack(&out)?;
    }
    Ok(())
}

fn download_repo_snapshot(repo: &str, dest: &Path, msg: &mut dyn FnMut(String)) -> Result<()> {
    msg(format!("Downloading repo snapshot via gh api ({})...", repo));
    let out = Command::new("gh").args(["api", &format!("repos/{}/tarball/HEAD", repo)]).output();
    if let Ok(out) = out {
        if out.status.success() && out.stdout.len() > 1000 {
            msg("Extracting snapshot...".into());
            extract_tar_gz(&out.stdout, dest)?;
            return Ok(());
        }
    }
    msg(format!("Cloning {} (shallow)...", repo));
    let tmp = dest.with_extension("tmpclone");
    let _ = fs::remove_dir_all(&tmp);
    let st = Command::new("git").args(["clone","--depth=1",&format!("https://github.com/{}.git", repo), tmp.to_string_lossy().as_ref()]).status();
    if st.map(|s| s.success()).unwrap_or(false) {
        fs::create_dir_all(dest)?;
        // simple recursive copy
        copy_dir_recursive(&tmp, dest)?;
        let _ = fs::remove_dir_all(&tmp);
        return Ok(());
    }
    Err(anyhow!("failed to obtain repository snapshot (install/auth gh or git)"))
}
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() { copy_dir_recursive(&entry.path(), &to)?; }
        else if ty.is_file() { fs::copy(entry.path(), &to)?; }
    }
    Ok(())
}

fn ensure_archive(cfg: &mut Config, msg: &mut dyn FnMut(String)) -> Result<()> {
    let data = PathBuf::from(&cfg.data_dir);
    let manifest = data.join("_meta/manifest.jsonl");
    if manifest.exists() { return Ok(()); }
    msg("Archive not found locally. Downloading...".into());
    // try latest release
    match latest_release(&cfg.repo) {
        Ok(rel) => {
            let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("latest").to_string();
            let asset_url = rel.get("assets").and_then(|a| a.as_array()).and_then(|arr| {
                arr.iter().find(|a| a.get("name").and_then(|n| n.as_str()).map(|n| (n.ends_with(".tar.gz")||n.ends_with(".tgz")) && n.contains("archive")).unwrap_or(false))
                .or_else(|| arr.iter().find(|a| a.get("name").and_then(|n| n.as_str()).map(|n| n.ends_with(".tar.gz")||n.ends_with(".tgz")).unwrap_or(false)))
                .and_then(|a| a.get("browser_download_url")).and_then(|v| v.as_str()).map(|s| s.to_string())
            });
            let url = asset_url.or_else(|| rel.get("tarball_url").and_then(|v| v.as_str()).map(|s| s.to_string()));
            if let Some(url) = url {
                msg(format!("Downloading release {}...", tag));
                let buf = http_get(&url)?;
                msg("Extracting...".into());
                extract_tar_gz(&buf, &data)?;
                cfg.installed_tag = Some(tag);
                return Ok(());
            }
            download_repo_snapshot(&cfg.repo, &data, msg)?;
        }
        Err(e) => {
            msg(format!("No release found ({}).", e));
            download_repo_snapshot(&cfg.repo, &data, msg)?;
        }
    }
    Ok(())
}

fn norm_local(lp: &str, data_dir: &Path) -> String {
    let mut rel = lp.to_string();
    let ds = data_dir.to_string_lossy().to_string();
    if rel.starts_with(&ds) { rel = rel[ds.len()..].trim_start_matches('/').to_string(); }
    else if let Some(i) = rel.find("/lwn-archive/") { rel = rel[i+"/lwn-archive/".len()..].to_string(); }
    rel.trim_start_matches('/').to_string()
}

fn incremental_update(cfg: &mut Config, msg: &mut dyn FnMut(String)) -> Result<()> {
    msg("Checking for updates...".into());
    let rel = match latest_release(&cfg.repo) { Ok(r)=>r, Err(e)=>{ msg(format!("No releases/update check failed ({}).", e)); return Ok(());} };
    let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if tag.is_empty() { return Ok(()); }
    if cfg.installed_tag.as_deref() == Some(tag.as_str()) { msg("Archive is up to date.".into()); return Ok(()); }
    msg(format!("New release {}. Fetching manifest...", tag));
    // fetch remote manifest
    let mut remote_manifest: Option<String> = None;
    if let Some(assets) = rel.get("assets").and_then(|a| a.as_array()) {
        if let Some(m) = assets.iter().find(|a| a.get("name").and_then(|n| n.as_str())==Some("manifest.jsonl")) {
            if let Some(u) = m.get("browser_download_url").and_then(|v| v.as_str()) { remote_manifest = http_get_string(u).ok(); }
        }
    }
    if remote_manifest.is_none() {
        for rref in [&tag, &"main".to_string()] {
            let url = format!("https://raw.githubusercontent.com/{}/{}/_meta/manifest.jsonl", cfg.repo, rref);
            if let Ok(s) = http_get_string(&url) { remote_manifest = Some(s); break; }
        }
    }
    let Some(remote_manifest) = remote_manifest else { msg("No remote manifest; skipping incremental update (re-run to try full sync).".into()); return Ok(()); };
    let data = PathBuf::from(&cfg.data_dir);
    let local_mpath = data.join("_meta/manifest.jsonl");
    let mut local_set: HashSet<String> = HashSet::new();
    if let Ok(s) = fs::read_to_string(&local_mpath) {
        for line in s.lines() {
            if let Ok(o) = serde_json::from_str::<serde_json::Value>(line) {
                if o.get("status").and_then(|v| v.as_str())==Some("ok") {
                    if let Some(lp) = o.get("local_path").and_then(|v| v.as_str()) { local_set.insert(norm_local(lp, &data)); }
                }
            }
        }
    }
    let mut new_count = 0usize; let mut err_count = 0usize; let mut checked = 0usize;
    let lines: Vec<&str> = remote_manifest.lines().filter(|l| !l.trim().is_empty()).collect();
    for line in &lines {
        checked += 1;
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
        if o.get("status").and_then(|v| v.as_str()) != Some("ok") { continue; }
        let Some(lp) = o.get("local_path").and_then(|v| v.as_str()) else { continue; };
        let relp = norm_local(lp, &data);
        if relp.is_empty() { continue; }
        if local_set.contains(&relp) && data.join(&relp).exists() { continue; }
        let raw = format!("https://raw.githubusercontent.com/{}/{}/{}", cfg.repo, tag, relp);
        match http_get(&raw) {
            Ok(buf) => {
                let dest = data.join(&relp);
                if let Some(p) = dest.parent() { let _ = fs::create_dir_all(p); }
                if fs::write(&dest, &buf).is_ok() { local_set.insert(relp); new_count += 1; }
                else { err_count += 1; }
            }
            Err(_) => { err_count += 1; }
        }
        if checked % 50 == 0 { msg(format!("Updating... {}/{} checked, {} new", checked, lines.len(), new_count)); }
    }
    if let Some(p) = local_mpath.parent() { let _ = fs::create_dir_all(p); }
    let _ = fs::write(&local_mpath, &remote_manifest);
    cfg.installed_tag = Some(tag);
    msg(format!("Incremental update: {} new files, {} errors.", new_count, err_count));
    Ok(())
}

fn category_from(rel: &str, title: &str) -> String {
    if rel.starts_with("Articles/") {
        if title.to_lowercase().contains("weekly edition") { return "Weekly Edition".into(); }
        if title.starts_with("Brief items") { return "Briefs".into(); }
        if title.starts_with("Announcements") { return "Announcements".into(); }
        if title.starts_with("Security updates") { return "Security".into(); }
        return "Articles".into();
    }
    if rel.starts_with("Kernel/") { return "Kernel".into(); }
    if rel.starts_with("Security/") { return "Security".into(); }
    if rel.starts_with("Archives/") { return "Archives".into(); }
    if rel.starts_with("Calendar/") { return "Calendar".into(); }
    if rel.starts_with("PythonIndex/") { return "Python".into(); }
    if rel.starts_with("current/") { return "Current".into(); }
    if rel.starts_with("op/") { return "LWN Info".into(); }
    if rel.starts_with("2001/") { return "2001 Archive".into(); }
    rel.split('/').next().unwrap_or("Other").to_string()
}
fn year_from(title: &str, ts: Option<f64>, rel: &str) -> String {
    for s in [title, rel] {
        for w in s.split(|c: char| !c.is_ascii_digit()) {
            if w.len()==4 { if let Ok(y)=w.parse::<i32>() { if (1990..=2100).contains(&y) { return w.to_string(); } } }
        }
    }
    if let Some(ts) = ts {
        // rough year
        let secs = ts as i64; let year = 1970 + secs / 31_557_600; if (1990..=2100).contains(&(year as i32)) { return year.to_string(); }
    }
    "Unknown".into()
}

fn parse_manifest(data_dir: &Path) -> Vec<Article> {
    let mut by_path: HashMap<String, Article> = HashMap::new();
    let mpath = data_dir.join("_meta/manifest.jsonl");
    if let Ok(s) = fs::read_to_string(&mpath) {
        for line in s.lines() {
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
            if o.get("status").and_then(|v| v.as_str()) != Some("ok") { continue; }
            let lp = o.get("local_path").and_then(|v| v.as_str()).unwrap_or("");
            let rel = norm_local(lp, data_dir);
            if !rel.ends_with(".html") { continue; }
            let title = o.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let title = if title.is_empty() { rel.rsplit('/').next().unwrap_or(&rel).to_string() } else { title };
            let ts = o.get("ts").and_then(|v| v.as_f64());
            let cat = category_from(&rel, &title);
            let year = year_from(&title, ts, &rel);
            by_path.insert(rel.clone(), Article { path: rel, title, category: cat, year });
        }
    }
    // scan Articles dir fallback
    if let Ok(rd) = fs::read_dir(data_dir.join("Articles")) {
        for e in rd.flatten() {
            let p = e.path(); if p.extension().and_then(|x| x.to_str()) != Some("html") { continue; }
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                let rel = format!("Articles/{}", name);
                if !by_path.contains_key(&rel) {
                    by_path.insert(rel.clone(), Article { path: rel, title: name.trim_end_matches(".html").to_string(), category: "Articles".into(), year: "Unknown".into() });
                }
            }
        }
    }
    let mut v: Vec<Article> = by_path.into_values().collect();
    v.sort_by(|a,b| b.year.cmp(&a.year).then(a.title.cmp(&b.title)));
    v
}

fn fuzzy_score(q: &str, text: &str) -> i32 {
    if q.is_empty() { return 1; }
    let ql = q.to_lowercase(); let tl = text.to_lowercase();
    if let Some(pos) = tl.find(&ql) { return 1000 - pos as i32; }
    // subsequence
    let qchars: Vec<char> = ql.chars().collect();
    let mut qi = 0usize; let mut score = 0i32; let mut last: i32 = -2;
    for (i,ch) in tl.chars().enumerate() {
        if qi < qchars.len() && ch == qchars[qi] { score += 10 - (i as i32 - last - 1).min(5); last = i as i32; qi += 1; }
    }
    if qi == qchars.len() { score } else { -1 }
}

fn strip_html(html: &str) -> String {
    let mut s = html.to_string();
    // very simple tag/section stripping
    for tag in ["script","style"] {
        let open = format!("<{}", tag); let close = format!("</{}>", tag);
        while let Some(i) = s.to_lowercase().find(&open) {
            if let Some(j) = s.to_lowercase()[i..].find(&close) { s.replace_range(i..i+j+close.len(), " "); } else { s.truncate(i); break; }
        }
    }
    s = s.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
    for t in ["</p>","</div>","</h1>","</h2>","</h3>","</h4>","</li>","</tr>","</section>","</article>","</header>","</blockquote>"] { s = s.replace(t, "\n"); }
    s = s.replace("<li>", " • ").replace("<li ", " • <li ");
    // remove tags
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch { '<'=> in_tag=true, '>'=> { in_tag=false; out.push(' '); }, _ if !in_tag => out.push(ch), _=>{} }
    }
    out = out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    let mut res: Vec<String> = Vec::new();
    let mut blank = 0;
    for line in out.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() { blank += 1; if blank <= 1 { res.push(String::new()); } }
        else { blank = 0; res.push(l); }
    }
    res.join("\n").trim().to_string()
}

#[derive(PartialEq, Eq)]
enum Screen { List, Reader, Bookmarks }
struct App {
    cfg: Config,
    state: AppState,
    bookmarks: Vec<Bookmark>,
    articles: Vec<Article>,
    filtered: Vec<usize>, // indices into articles
    categories: Vec<String>,
    years: Vec<String>,
    screen: Screen,
    list_state: ListState,
    filter: String,
    search_mode: bool,
    search_buf: String,
    category: String,
    year: String,
    pick_mode: Option<&'static str>, // "cat"|"year"
    pick_items: Vec<String>,
    pick_state: ListState,
    reader_article: Option<usize>,
    reader_lines: Vec<String>,
    reader_offset: u16,
    status: String,
    loading_msg: String,
}

impl App {
    fn new(cfg: Config, state: AppState, bookmarks: Vec<Bookmark>) -> Self {
        let filter = state.last_filter.clone(); let category = if state.last_category.is_empty(){"All".into()} else {state.last_category.clone()};
        let year = if state.last_year.is_empty(){"All".into()} else {state.last_year.clone()};
        Self { cfg, state, bookmarks, articles: vec![], filtered: vec![], categories: vec!["All".into()], years: vec!["All".into()], screen: Screen::List, list_state: ListState::default(), filter, search_mode: false, search_buf: String::new(), category, year, pick_mode: None, pick_items: vec![], pick_state: ListState::default(), reader_article: None, reader_lines: vec![], reader_offset: 0, status: String::new(), loading_msg: String::new() }
    }
    fn theme(&self) -> Theme { theme(if self.cfg.theme=="latte" { ThemeName::Latte } else { ThemeName::Mocha }) }
    fn refresh_index(&mut self) {
        self.articles = parse_manifest(Path::new(&self.cfg.data_dir));
        let mut cs = BTreeSet::new(); let mut ys = BTreeSet::new();
        for a in &self.articles { cs.insert(a.category.clone()); ys.insert(a.year.clone()); }
        self.categories = std::iter::once("All".into()).chain(cs.into_iter()).collect();
        let mut yv: Vec<String> = ys.into_iter().collect(); yv.sort(); yv.reverse();
        self.years = std::iter::once("All".into()).chain(yv).collect();
        self.apply_filter();
    }
    fn apply_filter(&mut self) {
        let q = self.filter.trim().to_string();
        let mut items: Vec<(i32, usize)> = Vec::new();
        for (i,a) in self.articles.iter().enumerate() {
            if self.category != "All" && a.category != self.category { continue; }
            if self.year != "All" && a.year != self.year { continue; }
            if q.is_empty() { items.push((0,i)); continue; }
            let hay = format!("{} {} {} {}", a.title, a.path, a.category, a.year);
            let sc = fuzzy_score(&q, &hay);
            if sc >= 0 { items.push((sc,i)); }
        }
        if !q.is_empty() { items.sort_by(|a,b| b.0.cmp(&a.0)); }
        self.filtered = items.into_iter().map(|(_,i)| i).collect();
        let len = self.filtered.len();
        if len == 0 { self.list_state.select(None); }
        else { let sel = self.list_state.selected().unwrap_or(self.state.last_list_index).min(len.saturating_sub(1)); self.list_state.select(Some(sel)); }
    }
    fn persist(&mut self) {
        self.state.last_view = if self.screen==Screen::Reader {"reader".into()} else {"list".into()};
        self.state.last_article_path = self.reader_article.and_then(|i| self.articles.get(i)).map(|a| a.path.clone());
        self.state.last_list_index = self.list_state.selected().unwrap_or(0);
        self.state.last_filter = self.filter.clone(); self.state.last_category = self.category.clone(); self.state.last_year = self.year.clone();
        self.state.reader_offset = self.reader_offset;
        let cdir = config_dir();
        let _ = save_json(&cdir.join("config.json"), &self.cfg);
        let _ = save_json(&cdir.join("state.json"), &self.state);
        let _ = save_json(&cdir.join("bookmarks.json"), &self.bookmarks);
    }
    fn open_reader(&mut self, art_idx: usize, offset: u16) {
        self.reader_article = Some(art_idx);
        self.reader_offset = offset;
        let path = Path::new(&self.cfg.data_dir).join(&self.articles[art_idx].path);
        let text = fs::read_to_string(&path).map(|h| strip_html(&h)).unwrap_or_else(|e| format!("Failed to open: {}", e));
        self.reader_lines = text.lines().map(|s| s.to_string()).collect();
        if self.reader_lines.is_empty() { self.reader_lines.push("(empty)".into()); }
        self.screen = Screen::Reader;
        self.persist();
    }
    fn toggle_bookmark_current(&mut self) {
        let idx = match self.screen { Screen::Reader => self.reader_article, _ => self.list_state.selected().and_then(|s| self.filtered.get(s).copied()) };
        if let Some(i) = idx { let a = &self.articles[i]; if let Some(p)=self.bookmarks.iter().position(|b| b.path==a.path){ self.bookmarks.remove(p); self.status="Bookmark removed".into(); } else { let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(); self.bookmarks.insert(0, Bookmark{path:a.path.clone(), title:a.title.clone(), added_at: ts.to_string()}); self.status="Bookmarked".into(); } self.persist(); }
    }
}

fn draw_loading(term: &mut Terminal<CrosstermBackend<io::Stdout>>, msg: &str, th: &Theme) -> Result<()> {
    term.draw(|f| {
        let area = f.area();
        let block = Block::default().style(Style::default().bg(th.base).fg(th.text));
        f.render_widget(block, area);
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(1)]).split(area);
        let p = Paragraph::new(vec![Line::from(Span::styled(" LWN Reader ", Style::default().fg(th.base).bg(th.mauve).add_modifier(Modifier::BOLD))), Line::from(""), Line::from(Span::styled(msg, Style::default().fg(th.text)))]).style(Style::default().bg(th.base));
        f.render_widget(p, chunks[0].union(chunks[1]));
    })?;
    Ok(())
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let th = app.theme();
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(th.base).fg(th.text)), area);
    match app.screen {
        Screen::List => draw_list(f, app, &th, area),
        Screen::Reader => draw_reader(f, app, &th, area),
        Screen::Bookmarks => draw_bookmarks(f, app, &th, area),
    }
    if app.pick_mode.is_some() { draw_picker(f, app, &th, area); }
}
fn draw_list(f: &mut ratatui::Frame, app: &mut App, th: &Theme, area: Rect) {
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let title = format!(" LWN Reader  [{}]  theme:{} ", app.cfg.repo, app.cfg.theme);
    f.render_widget(Paragraph::new(title).style(Style::default().fg(th.base).bg(th.blue).add_modifier(Modifier::BOLD)), chunks[0]);
    let info = format!(" Category: {}  Year: {}  Filter: {}  ({}/{}) ", app.category, app.year, if app.filter.is_empty(){"(none)"} else {&app.filter}, app.filtered.len(), app.articles.len());
    f.render_widget(Paragraph::new(info).style(Style::default().fg(th.text).bg(th.surface0)), chunks[1]);
    let bm: HashSet<&str> = app.bookmarks.iter().map(|b| b.path.as_str()).collect();
    let items: Vec<ListItem> = app.filtered.iter().map(|&i| {
        let a = &app.articles[i]; let star = if bm.contains(a.path.as_str()) {"★ "} else {"  "};
        ListItem::new(format!("{}{}  [{}] {}", star, a.year, a.category, a.title))
    }).collect();
    let list = List::new(items).style(Style::default().fg(th.text).bg(th.base)).highlight_style(Style::default().fg(th.lavender).bg(th.surface1).add_modifier(Modifier::BOLD)).highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[2], &mut app.list_state);
    let help = if app.search_mode { format!(" / {}█  (Enter apply, Esc cancel)", app.search_buf) }
        else if !app.status.is_empty() { app.status.clone() }
        else { " j/k:move Enter:read /:fuzzy c:category y:year b:bookmarks B:toggle★ u:update t:theme q:quit ".into() };
    f.render_widget(Paragraph::new(help).style(Style::default().fg(th.subtext).bg(th.mantle)), chunks[3]);
}
fn draw_reader(f: &mut ratatui::Frame, app: &mut App, th: &Theme, area: Rect) {
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (title, meta) = if let Some(i)=app.reader_article { let a=&app.articles[i]; (format!(" {}", a.title), format!(" {} | {} | {}", a.category, a.year, a.path)) } else { (" Reader".into(), String::new()) };
    f.render_widget(Paragraph::new(title).style(Style::default().fg(th.base).bg(th.mauve).add_modifier(Modifier::BOLD)), chunks[0]);
    f.render_widget(Paragraph::new(meta).style(Style::default().fg(th.subtext).bg(th.surface0)), chunks[1]);
    let view_h = chunks[2].height.saturating_sub(0) as usize;
    let max_off = app.reader_lines.len().saturating_sub(view_h);
    let off = (app.reader_offset as usize).min(max_off);
    app.reader_offset = off as u16;
    let visible: Vec<Line> = app.reader_lines.iter().skip(off).take(view_h).map(|l| Line::from(l.as_str())).collect();
    f.render_widget(Paragraph::new(visible).style(Style::default().fg(th.text).bg(th.base)).wrap(Wrap{trim:false}), chunks[2]);
    let pct = if app.reader_lines.len()<=view_h {100} else { (off*100)/max_off.max(1) };
    f.render_widget(Paragraph::new(format!(" q/esc:back j/k:scroll space/b:page g/G:top/end B:bookmark  {}% ", pct)).style(Style::default().fg(th.subtext).bg(th.mantle)), chunks[3]);
}
fn draw_bookmarks(f: &mut ratatui::Frame, app: &mut App, th: &Theme, area: Rect) {
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(area);
    f.render_widget(Paragraph::new(" Bookmarks ").style(Style::default().fg(th.base).bg(th.peach).add_modifier(Modifier::BOLD)), chunks[0]);
    let items: Vec<ListItem> = app.bookmarks.iter().map(|b| ListItem::new(format!("★ {}  ({})", b.title, b.path))).collect();
    let list = List::new(items).style(Style::default().fg(th.text).bg(th.base)).highlight_style(Style::default().fg(th.lavender).bg(th.surface1).add_modifier(Modifier::BOLD)).highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut app.list_state);
    f.render_widget(Paragraph::new(" Enter:open d:delete q/esc:back ").style(Style::default().fg(th.subtext).bg(th.mantle)), chunks[2]);
}
fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w)/2; let y = area.y + area.height.saturating_sub(h)/2;
    Rect::new(x,y,w.min(area.width), h.min(area.height))
}
fn draw_picker(f: &mut ratatui::Frame, app: &mut App, th: &Theme, area: Rect) {
    let w = 40u16.min(area.width.saturating_sub(4)); let h = (app.pick_items.len() as u16 + 2).min(area.height.saturating_sub(4));
    let rect = centered_rect(w,h,area);
    f.render_widget(Clear, rect);
    let items: Vec<ListItem> = app.pick_items.iter().map(|s| ListItem::new(s.as_str())).collect();
    let title = if app.pick_mode==Some("cat") {" Category "} else {" Year "};
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title).style(Style::default().bg(th.surface0).fg(th.text))).highlight_style(Style::default().fg(th.lavender).bg(th.surface1).add_modifier(Modifier::BOLD)).highlight_symbol("> ");
    f.render_stateful_widget(list, rect, &mut app.pick_state);
}

fn main() -> Result<()> {
    let mut cfg: Config = {
        let mut c: Config = load_json(&config_dir().join("config.json"));
        // env overrides
        if let Ok(r) = std::env::var("LWN_ARCHIVE_REPO") { if !r.is_empty() { c.repo = r; } }
        if let Ok(d) = std::env::var("LWN_ARCHIVE_DIR") { if !d.is_empty() { c.data_dir = d; } }
        // args
        let args: Vec<String> = std::env::args().collect();
        let mut i=1; while i<args.len() { match args[i].as_str() { "--data-dir"=>{ if i+1<args.len(){c.data_dir=args[i+1].clone(); i+=1;} }, "--repo"=>{ if i+1<args.len(){c.repo=args[i+1].clone(); i+=1;} }, "--theme"=>{ if i+1<args.len(){c.theme=args[i+1].clone(); i+=1;} }, _=>{} } i+=1; }
        c
    };
    let state: AppState = load_json(&config_dir().join("state.json"));
    let bookmarks: Vec<Bookmark> = load_json(&config_dir().join("bookmarks.json"));
    let _ = fs::create_dir_all(&cfg.data_dir);
    let _ = fs::create_dir_all(config_dir());
    let _ = save_json(&config_dir().join("config.json"), &cfg);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let th = theme(if cfg.theme=="latte" { ThemeName::Latte } else { ThemeName::Mocha });
    draw_loading(&mut terminal, "Starting...", &th)?;
    let mut log = |m: String| { let _ = draw_loading(&mut terminal, &m, &th); };
    let _ = ensure_archive(&mut cfg, &mut log);
    let _ = incremental_update(&mut cfg, &mut log);

    let mut app = App::new(cfg, state, bookmarks);
    app.refresh_index();
    // restore session
    let restore_path = app.state.last_article_path.clone();
    let restore_off = app.state.reader_offset;
    if app.state.last_view == "reader" {
        if let Some(p) = restore_path { if let Some(idx)=app.articles.iter().position(|a| a.path==p) { app.open_reader(idx, restore_off); } }
    } else {
        if !app.filtered.is_empty() { let sel = app.state.last_list_index.min(app.filtered.len()-1); app.list_state.select(Some(sel)); }
    }
    app.persist();

    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        app.status.clear();
        terminal.draw(|f| ui(f, app))?;
        if !event::poll(Duration::from_millis(250))? { continue; }
        match event::read()? {
            Event::Key(k) => { if handle_key(app, k, terminal)? { break; } }
            Event::Resize(_,_) => {}
            _ => {}
        }
    }
    app.persist();
    Ok(())
}

fn handle_key(app: &mut App, k: KeyEvent, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<bool> {
    // picker mode
    if let Some(mode) = app.pick_mode {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => { app.pick_mode = None; }
            KeyCode::Down | KeyCode::Char('j') => { let n=app.pick_items.len(); if n>0 { let i=app.pick_state.selected().unwrap_or(0); app.pick_state.select(Some((i+1).min(n-1))); } }
            KeyCode::Up | KeyCode::Char('k') => { let i=app.pick_state.selected().unwrap_or(0); app.pick_state.select(Some(i.saturating_sub(1))); }
            KeyCode::Enter => { let sel = app.pick_state.selected().unwrap_or(0); if let Some(val)=app.pick_items.get(sel).cloned() { if mode=="cat" { app.category=val; } else { app.year=val; } } app.pick_mode=None; app.apply_filter(); app.persist(); }
            _ => {}
        }
        return Ok(false);
    }
    match app.screen {
        Screen::List => {
            if app.search_mode {
                match k.code {
                    KeyCode::Esc => { app.search_mode=false; app.search_buf.clear(); }
                    KeyCode::Enter => { app.search_mode=false; app.filter = app.search_buf.clone(); app.apply_filter(); app.persist(); }
                    KeyCode::Backspace => { app.search_buf.pop(); app.filter = app.search_buf.clone(); app.apply_filter(); }
                    KeyCode::Char(c) => { if !k.modifiers.contains(KeyModifiers::CONTROL) { app.search_buf.push(c); app.filter = app.search_buf.clone(); app.apply_filter(); } }
                    _ => {}
                }
                return Ok(false);
            }
            match k.code {
                KeyCode::Char('q') => { app.persist(); return Ok(true); }
                KeyCode::Down | KeyCode::Char('j') => { let n=app.filtered.len(); if n>0 { let i=app.list_state.selected().unwrap_or(0); app.list_state.select(Some((i+1).min(n-1))); } }
                KeyCode::Up | KeyCode::Char('k') => { let i=app.list_state.selected().unwrap_or(0); app.list_state.select(Some(i.saturating_sub(1))); }
                KeyCode::Char('g') => { if !app.filtered.is_empty() { app.list_state.select(Some(0)); } }
                KeyCode::Char('G') => { if !app.filtered.is_empty() { app.list_state.select(Some(app.filtered.len()-1)); } }
                KeyCode::Enter => { if let Some(sel)=app.list_state.selected() { if let Some(&ai)=app.filtered.get(sel) { app.open_reader(ai,0); } } }
                KeyCode::Char('/') => { app.search_mode=true; app.search_buf = app.filter.clone(); }
                KeyCode::Char('c') => { app.pick_mode=Some("cat"); app.pick_items = app.categories.clone(); let idx = app.pick_items.iter().position(|x| x==&app.category).unwrap_or(0); app.pick_state = ListState::default(); app.pick_state.select(Some(idx)); }
                KeyCode::Char('y') => { app.pick_mode=Some("year"); app.pick_items = app.years.clone(); let idx = app.pick_items.iter().position(|x| x==&app.year).unwrap_or(0); app.pick_state = ListState::default(); app.pick_state.select(Some(idx)); }
                KeyCode::Char('t') => { app.cfg.theme = if app.cfg.theme=="mocha" {"latte".into()} else {"mocha".into()}; app.persist(); }
                KeyCode::Char('B') => { app.toggle_bookmark_current(); }
                KeyCode::Char('b') => { app.screen=Screen::Bookmarks; app.list_state = ListState::default(); if !app.bookmarks.is_empty() { app.list_state.select(Some(0)); } }
                KeyCode::Char('u') => {
                    let th = app.theme(); draw_loading(terminal, "Updating...", &th)?; let mut log = |m: String| { let _ = draw_loading(terminal, &m, &th); };
                    let _ = incremental_update(&mut app.cfg, &mut log); app.refresh_index(); app.persist();
                }
                KeyCode::Char(' ') => { let n=app.filtered.len(); if n>0 { let i=app.list_state.selected().unwrap_or(0); let step = terminal.size()?.height.saturating_sub(6) as usize; app.list_state.select(Some((i+step).min(n-1))); } }
                _ => {}
            }
        }
        Screen::Reader => {
            let view_h = terminal.size()?.height.saturating_sub(4) as usize;
            let max_off = app.reader_lines.len().saturating_sub(view_h) as u16;
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => { app.screen=Screen::List; app.persist(); }
                KeyCode::Down | KeyCode::Char('j') => { app.reader_offset = (app.reader_offset+1).min(max_off); app.persist(); }
                KeyCode::Up | KeyCode::Char('k') => { app.reader_offset = app.reader_offset.saturating_sub(1); app.persist(); }
                KeyCode::Char(' ') => { app.reader_offset = (app.reader_offset + view_h as u16).min(max_off); app.persist(); }
                KeyCode::Char('b') => { if k.modifiers.contains(KeyModifiers::CONTROL) || !k.modifiers.contains(KeyModifiers::CONTROL) { app.reader_offset = app.reader_offset.saturating_sub(view_h as u16); app.persist(); } }
                KeyCode::Char('g') => { app.reader_offset = 0; app.persist(); }
                KeyCode::Char('G') => { app.reader_offset = max_off; app.persist(); }
                KeyCode::Char('B') => { app.toggle_bookmark_current(); }
                _ => {}
            }
        }
        Screen::Bookmarks => {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => { app.screen=Screen::List; app.list_state=ListState::default(); if !app.filtered.is_empty(){app.list_state.select(Some(0));} }
                KeyCode::Down | KeyCode::Char('j') => { let n=app.bookmarks.len(); if n>0 { let i=app.list_state.selected().unwrap_or(0); app.list_state.select(Some((i+1).min(n-1))); } }
                KeyCode::Up | KeyCode::Char('k') => { let i=app.list_state.selected().unwrap_or(0); app.list_state.select(Some(i.saturating_sub(1))); }
                KeyCode::Char('d') => { if let Some(i)=app.list_state.selected() { if i < app.bookmarks.len() { app.bookmarks.remove(i); let n=app.bookmarks.len(); if n==0 { app.list_state.select(None);} else { app.list_state.select(Some(i.min(n-1))); } app.persist(); } } }
                KeyCode::Enter => { if let Some(i)=app.list_state.selected() { if let Some(b)=app.bookmarks.get(i).cloned() { if let Some(ai)=app.articles.iter().position(|a| a.path==b.path) { app.open_reader(ai,0); } else { app.status = "Bookmarked article not found locally; press u to update.".into(); } } } }
                _ => {}
            }
        }
    }
    Ok(false)
}
