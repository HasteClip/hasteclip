//! Share links: a clip uploaded to HasteClip's server, with a link that plays
//! it in any browser and right in a Discord chat (the server is
//! <https://github.com/hasteclip/api>).
//!
//! Nothing leaves the computer until you pick Share → Share with a link… and
//! Upload, and then only that clip as it looks now: its video and its mix, without
//! the file's metadata (repackaged, not re-encoded, so it's quick), and a
//! picture for the link's preview. The server knows this app only by a
//! random token it hands out the first time (kept in
//! `<config_dir>/hasteclip/share.json`): there's no account.
//!
//! Shared clips are kept in the library's `.hasteclip/links.json`, by the
//! clip's path inside the library, like collections: renaming or moving a
//! clip keeps its link. Stop sharing deletes the clip from the server.
//!
//! A link shows its clip as it is now, as a shared file does in a cloud
//! drive: the clip on this computer is the real one, the server's a copy
//! kept up to date. Renamed (or moved to another game), its page is
//! renamed; edited, reverted or saved smaller, the new version is uploaded
//! under the same link (which plays the old one until it's all there);
//! deleted, its link stops working, once Undo's had its moment. Without a
//! connection, all of that waits and happens once there is one.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::App;
use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Dismissed as _, Kind};

/// The server. `HASTECLIP_API` points the app at another one (a local one
/// while working on it).
fn api() -> String {
    std::env::var("HASTECLIP_API").unwrap_or_else(|_| "https://api.hasteclip.com".into())
}

/// A shared clip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub id: String,
    pub url: String,
    /// When it was shared, in seconds since 1970.
    pub shared: u64,
    /// What the server has: the name and game on its page, and which file
    /// it was made from. `None` in links kept before these were: taken to
    /// be the clip as it is.
    #[serde(default)]
    pub named: Option<Named>,
    #[serde(default)]
    pub version: Option<Version>,
}

/// What a shared clip's page calls it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Named {
    pub title: String,
    pub game: Option<String>,
}

/// Which file a clip is: its size and when the file was made. A saved
/// edit, a revert or a smaller copy is a new file (its date in the library
/// is kept, so not the modified time); a rename or move is the same one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Version {
    size: u64,
    /// Milliseconds since 1970, where the file system keeps it.
    made: Option<u64>,
}

impl Version {
    pub fn of(clip: &Path) -> Option<Self> {
        let meta = std::fs::metadata(clip).ok()?;
        let made = meta.created().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
        Some(Self { size: meta.len(), made })
    }
}

/// A shared clip deleted here: its link is stopped once Undo has had its
/// moment ([`UNDO_FOR`]), as soon as the server can be reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Deleted {
    /// Where the clip was in the library.
    key: String,
    link: Link,
    /// When, in seconds since 1970.
    at: u64,
}

/// How long a deleted clip's link waits before it's stopped: past the
/// Undo in the toast, so Undo brings the link back too.
const UNDO_FOR: Duration = Duration::from_secs(20);

#[derive(Default, Serialize, Deserialize)]
struct File {
    clips: HashMap<String, Link>,
    #[serde(default)]
    deleted: Vec<Deleted>,
}

/// The shared clips of one library.
#[derive(Default)]
pub struct Links {
    lib: PathBuf,
    clips: HashMap<String, Link>,
    deleted: Vec<Deleted>,
    /// Shared clips whose link shows an older version or name, as of the
    /// last look (for their cards, without looking at their files every frame).
    behind: std::collections::HashSet<String>,
    /// Links that couldn't be brought up to date, by id: when to try again,
    /// and how many tries so far.
    retry: HashMap<String, (Instant, u32)>,
    /// When the clips were last looked at for changes.
    looked: Option<Instant>,
    /// Goes up with every change to which clips are shared.
    pub changes: u64,
}

impl Links {
    pub fn load(lib: &Path) -> Self {
        let file: File = std::fs::read(file_path(lib)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { lib: lib.to_path_buf(), clips: file.clips, deleted: file.deleted, ..Default::default() }
    }

    pub fn library(&self) -> &Path {
        &self.lib
    }

    pub fn get(&self, clip: &Path) -> Option<&Link> {
        self.clips.get(&self.key(clip)?)
    }

    pub fn set(&mut self, clip: &Path, link: Link) {
        let Some(key) = self.key(clip) else { return };
        self.behind.remove(&key);
        self.retry.remove(&link.id);
        self.clips.insert(key, link);
        self.changes += 1;
        self.save();
    }

    pub fn remove(&mut self, clip: &Path) {
        if let Some(key) = self.key(clip)
            && self.clips.remove(&key).is_some()
        {
            self.behind.remove(&key);
            self.changes += 1;
            self.save();
        }
    }

    /// The shared clips, by where they are.
    pub fn all(&self) -> Vec<(PathBuf, Link)> {
        self.clips.iter().map(|(key, link)| (self.lib.join(key), link.clone())).collect()
    }

    /// Whether the clip's link shows an older version or name of it.
    pub fn behind(&self, clip: &Path) -> bool {
        self.key(clip).is_some_and(|k| self.behind.contains(&k))
    }

    /// The clip was deleted here: its link waits [`UNDO_FOR`], then stops.
    pub fn deleted(&mut self, clip: &Path) {
        let Some(key) = self.key(clip) else { return };
        if let Some(link) = self.clips.remove(&key) {
            self.behind.remove(&key);
            self.deleted.push(Deleted { key, link, at: now() });
            self.changes += 1;
            self.save();
        }
    }

    /// The clip was put back (Undo): its link stays, if it hasn't stopped yet.
    pub fn undeleted(&mut self, clip: &Path) {
        let Some(key) = self.key(clip) else { return };
        if let Some(i) = self.deleted.iter().position(|d| d.key == key) {
            let d = self.deleted.remove(i);
            self.clips.insert(key, d.link);
            self.changes += 1;
            self.save();
        }
    }

    /// A deleted clip's link has stopped (or was gone already): forgotten.
    fn forget_deleted(&mut self, id: &str) {
        self.retry.remove(id);
        let before = self.deleted.len();
        self.deleted.retain(|d| d.link.id != id);
        if self.deleted.len() != before {
            self.save();
        }
    }

    /// Bringing the link with `id` up to date didn't work: it's tried again
    /// later, a little later each time.
    fn failed(&mut self, id: &str) {
        let tries = self.retry.get(id).map_or(0, |(_, n)| *n);
        self.retry.insert(id.to_owned(), (Instant::now() + backoff(tries), tries + 1));
    }

    /// Bring a link up to date again now, not when it would next try.
    pub fn try_now(&mut self, clip: &Path) {
        if let Some(link) = self.get(clip) {
            let id = link.id.clone();
            self.retry.remove(&id);
            self.looked = None;
        }
    }

    /// A clip was renamed or moved: its link goes with it (and is renamed
    /// on the server soon after).
    pub fn follow(&mut self, from: &Path, to: &Path) {
        let (Some(from), Some(to)) = (self.key(from), self.key(to)) else { return };
        if let Some(link) = self.clips.remove(&from) {
            self.behind.remove(&from);
            self.clips.insert(to, link);
            self.changes += 1;
            self.looked = None;
            self.save();
        }
    }

    fn key(&self, clip: &Path) -> Option<String> {
        let rel = clip.strip_prefix(&self.lib).ok()?;
        Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
    }

    fn save(&self) {
        let path = file_path(&self.lib);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File { clips: self.clips.clone(), deleted: self.deleted.clone() };
        if let Ok(json) = serde_json::to_vec_pretty(&file) {
            // Written aside, then swapped in: never half a file.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Links to another server (`HASTECLIP_API`) are kept apart, like its token.
fn file_path(lib: &Path) -> PathBuf {
    let name = if std::env::var_os("HASTECLIP_API").is_some() { "links-dev.json" } else { "links.json" };
    lib.join(crate::store::DIR).join(name)
}

/// How far a share has come: bytes sent of the total (0 while the clip is
/// still being got ready), and whether it was called off.
#[derive(Default)]
pub struct Progress {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    /// 0..=1.
    pub fn fraction(&self) -> f32 {
        let (sent, total) = (self.sent.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        if total == 0 { 0.0 } else { (sent as f64 / total as f64).min(1.0) as f32 }
    }

    fn cancelled(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) { Err("cancelled".into()) } else { Ok(()) }
    }
}

/// Upload `clip` and get its link; `named` is what its page calls it.
pub fn share(clip: &Path, named: &Named, progress: &Progress) -> Result<Link, String> {
    // Which file, as it was before it was read: if it changes meanwhile,
    // the link is brought up to date after.
    let version = Version::of(clip);
    let done = upload(clip, named, progress, None)?;
    Ok(Link { id: done.id, url: done.url, shared: now(), named: Some(named.clone()), version })
}

/// Upload a new version of a shared clip under its link, which plays the
/// old one until this one's all there.
pub fn replace(clip: &Path, link: &Link, named: &Named, progress: &Progress) -> Result<Link, String> {
    let version = Version::of(clip);
    upload(clip, named, progress, Some(&link.id))?;
    Ok(Link { named: Some(named.clone()), version, ..link.clone() })
}

/// Rename a shared clip's page, or give it another game.
pub fn rename(link: &Link, named: &Named) -> Result<(), String> {
    let server = Server::new();
    server
        .authed_raw(|s, token| {
            s.agent
                .patch(format!("{}/v1/clips/{}", api(), link.id))
                .header("Authorization", format!("Bearer {token}"))
                .send_json(serde_json::json!({ "title": named.title, "game": named.game }))
        })
        .map(drop)
}

/// Upload `clip` as a new shared clip, or as the new version of shared clip
/// `replacing`.
fn upload(clip: &Path, named: &Named, progress: &Progress, replacing: Option<&str>) -> Result<Shared, String> {
    let work = Scratch::new()?;
    let video = work.0.join("video.mp4");
    web_copy(clip, &video)?;
    progress.cancelled()?;
    let info = media::probe(&video).map_err(|e| e.to_string())?;
    let thumb = work.0.join("thumb.jpg");
    let thumb = picture(&video, info.duration, &thumb).then_some(thumb);
    let size = std::fs::metadata(&video).map_err(|e| e.to_string())?.len();
    let thumb_bytes = thumb.as_ref().and_then(|t| std::fs::read(t).ok());

    let server = Server::new();
    let start = match replacing {
        None => format!("{}/v1/clips", api()),
        Some(id) => format!("{}/v1/clips/{id}/replace", api()),
    };
    let upload: Upload = server.authed(|s, token| {
        s.agent.post(&start).header("Authorization", format!("Bearer {token}")).send_json(serde_json::json!({
            "size": size,
            "duration_ms": (info.duration * 1000.0).round() as i64,
            "width": info.width,
            "height": info.height,
            "fps": info.fps,
            "title": named.title,
            "game": named.game,
            "thumb_size": thumb_bytes.as_ref().map(Vec::len),
        }))
    })?;
    progress.total.store(size + thumb_bytes.as_ref().map_or(0, |b| b.len() as u64), Ordering::Relaxed);

    let uploaded = (|| {
        let etags = put_parts(&server, &video, size, &upload, progress)?;
        progress.cancelled()?;
        if let (Some(link), Some(bytes)) = (&upload.thumb, &thumb_bytes) {
            // The preview picture is nice to have, not worth failing over.
            if server.put(link, bytes, Some("image/jpeg")).is_ok() {
                progress.sent.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
        }
        let complete = match replacing {
            None => format!("{}/v1/clips/{}/complete", api(), upload.id),
            Some(id) => format!("{}/v1/clips/{id}/replace/complete", api()),
        };
        server.authed::<Shared>(|s, token| s.agent.post(&complete).header("Authorization", format!("Bearer {token}")).send_json(serde_json::json!({ "etags": etags })))
    })();
    // Don't leave half a clip counting against what can be shared. (Half a
    // new version is dropped by the server, or by the next one.)
    if uploaded.is_err() && replacing.is_none() {
        let _ = delete(&server, &upload.id);
    }
    uploaded
}

/// Delete the clip from the server: the link stops working.
pub fn unshare(link: &Link) -> Result<(), String> {
    delete(&Server::new(), &link.id)
}

/// What the server says of a shared clip.
#[derive(Debug, PartialEq)]
pub enum Status {
    /// Still shared: how many times its page was opened.
    Shared { views: i64 },
    /// Gone: the server takes down clips nobody has watched in a long while,
    /// and ones that break its rules.
    Gone,
}

/// Ask the server about the clip. `None` when it can't be asked.
pub fn status(link: &Link) -> Option<Status> {
    let server = Server::new();
    let token = server.saved_token()?;
    let mut res = server.agent.get(format!("{}/v1/clips", api())).header("Authorization", format!("Bearer {token}")).call().ok()?;
    if !res.status().is_success() {
        return None;
    }
    #[derive(Deserialize)]
    struct Listed {
        id: String,
        state: String,
        views: i64,
    }
    let clips: Vec<Listed> = res.body_mut().read_json().ok()?;
    Some(clips.iter().find(|c| c.id == link.id && c.state == "ready").map_or(Status::Gone, |c| Status::Shared { views: c.views }))
}

fn delete(server: &Server, id: &str) -> Result<(), String> {
    let Some(token) = server.saved_token() else {
        return Err("it was shared from another install of HasteClip".into());
    };
    let res = server.agent.delete(format!("{}/v1/clips/{id}", api())).header("Authorization", format!("Bearer {token}")).call();
    match res {
        Ok(r) if r.status().is_success() || r.status() == 404 => Ok(()),
        Ok(mut r) => Err(error_of(&mut r)),
        Err(e) => Err(offline(e)),
    }
}

/// The clip as a browser plays it: an MP4 with its video and mix only, the
/// moov box at the front (it starts playing before it's all downloaded),
/// and no metadata (the library's own tags stay home).
fn web_copy(clip: &Path, out: &Path) -> Result<(), String> {
    let probe = media::ffprobe()
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0"])
        .arg(clip)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let streams: Vec<(String, String)> = String::from_utf8_lossy(&probe.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(',').map(str::trim);
            let (a, b) = (f.next()?, f.next()?);
            // Whichever order ffprobe prints them in.
            Some(if a == "video" || a == "audio" { (a.to_owned(), b.to_owned()) } else { (b.to_owned(), a.to_owned()) })
        })
        .collect();
    let video = streams.iter().find(|(t, _)| t == "video").map(|(_, c)| c.as_str());
    let audio = streams.iter().find(|(t, _)| t == "audio").map(|(_, c)| c.as_str());
    if video != Some("h264") {
        return Err("its video isn't H.264, which browsers need. Save a copy with Edit → Save smaller and share that".into());
    }
    if audio.is_some_and(|a| a != "aac") {
        return Err("its sound isn't AAC, which browsers need. Save a copy with Edit → Save smaller and share that".into());
    }
    let done = media::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(clip)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-dn", "-sn", "-movflags", "+faststart", "-f", "mp4"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if done.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&done.stderr);
    Err(why.lines().last().unwrap_or("ffmpeg failed").to_owned())
}

/// The picture a link's preview shows: a frame a second in (or from the
/// middle of a shorter clip), at most 1280 wide. `false` if there isn't one.
fn picture(video: &Path, duration: f64, out: &Path) -> bool {
    let at = (duration / 2.0).min(1.0);
    media::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &format!("{at:.3}"), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale='min(1280,iw)':-2", "-q:v", "3"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        && std::fs::metadata(out).is_ok_and(|m| m.len() > 0 && m.len() < 5 << 20)
}

/// What the server answers a new share with: where to upload each part.
#[derive(Deserialize)]
struct Upload {
    id: String,
    part_size: u64,
    parts: Vec<String>,
    thumb: Option<String>,
}

#[derive(Deserialize)]
struct Shared {
    id: String,
    url: String,
}

/// Parts going up at once: enough to fill a connection that's far from the
/// server, few enough to be gentle on a slow one.
const PARALLEL: usize = 3;

/// Upload the video's parts, a few at a time; their ETags, in order.
fn put_parts(server: &Server, video: &Path, size: u64, upload: &Upload, progress: &Progress) -> Result<Vec<String>, String> {
    let n = upload.parts.len();
    let next = AtomicUsize::new(0);
    let etags: Mutex<Vec<Option<String>>> = Mutex::new(vec![None; n]);
    let failed: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..PARALLEL.min(n) {
            s.spawn(|| {
                let mut file = match std::fs::File::open(video) {
                    Ok(f) => f,
                    Err(e) => return *failed.lock().unwrap() = Some(e.to_string()),
                };
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || failed.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(e) = progress.cancelled() {
                        return *failed.lock().unwrap() = Some(e);
                    }
                    let start = i as u64 * upload.part_size;
                    let len = upload.part_size.min(size - start);
                    buf.resize(len as usize, 0);
                    if let Err(e) = file.seek(SeekFrom::Start(start)).and_then(|_| file.read_exact(&mut buf)) {
                        return *failed.lock().unwrap() = Some(e.to_string());
                    }
                    match server.put(&upload.parts[i], &buf, None) {
                        Ok(etag) => {
                            etags.lock().unwrap()[i] = Some(etag);
                            progress.sent.fetch_add(len, Ordering::Relaxed);
                        }
                        Err(e) => return *failed.lock().unwrap() = Some(e),
                    }
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    etags.into_inner().unwrap().into_iter().map(|e| e.ok_or_else(|| "a part of the clip didn't upload".to_owned())).collect()
}

struct Server {
    /// For the API: quick answers.
    agent: ureq::Agent,
    /// For uploading parts: 8 MB on a slow connection takes a while.
    upload: ureq::Agent,
}

impl Server {
    fn new() -> Self {
        let agent = |secs| ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).http_status_as_error(false).build().into();
        Self { agent: agent(30), upload: agent(600) }
    }

    /// This app's token, signing up for one the first time.
    fn token(&self) -> Result<String, String> {
        if let Some(t) = self.saved_token() {
            return Ok(t);
        }
        let mut res = self.agent.post(format!("{}/v1/uploaders", api())).send_empty().map_err(offline)?;
        if !res.status().is_success() {
            return Err(error_of(&mut res));
        }
        #[derive(Deserialize)]
        struct Token {
            token: String,
        }
        let token = res.body_mut().read_json::<Token>().map_err(|e| e.to_string())?.token;
        let path = token_path().ok_or("no settings folder")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, serde_json::json!({ "token": token }).to_string()).map_err(|e| e.to_string())?;
        Ok(token)
    }

    fn saved_token(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct Saved {
            token: String,
        }
        let saved: Saved = serde_json::from_slice(&std::fs::read(token_path()?).ok()?).ok()?;
        Some(saved.token)
    }

    /// A request with the token, read as `T`.
    fn authed<T: serde::de::DeserializeOwned>(
        &self,
        send: impl Fn(&Self, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<T, String> {
        self.authed_raw(send)?.body_mut().read_json().map_err(|e| e.to_string())
    }

    /// A request with the token, answered with success. A token the server
    /// doesn't know (it forgot it) is replaced by a new one, once. A clip it
    /// doesn't have is [`NOT_THERE`].
    fn authed_raw(&self, send: impl Fn(&Self, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<ureq::http::Response<ureq::Body>, String> {
        for retry in [false, true] {
            let token = self.token()?;
            let mut res = send(self, &token).map_err(offline)?;
            if res.status() == 401 && !retry {
                if let Some(p) = token_path() {
                    let _ = std::fs::remove_file(p);
                }
                continue;
            }
            if res.status() == 404 {
                return Err(NOT_THERE.into());
            }
            if !res.status().is_success() {
                return Err(error_of(&mut res));
            }
            return Ok(res);
        }
        Err("the server didn't take this app's token".into())
    }

    /// `PUT` bytes to a signed link, trying again on a hiccup; the ETag.
    /// `content_type` when the link was signed for one.
    fn put(&self, link: &str, bytes: &[u8], content_type: Option<&str>) -> Result<String, String> {
        let mut last = String::new();
        for attempt in 0..4 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1 << attempt));
            }
            let mut req = self.upload.put(link);
            if let Some(t) = content_type {
                req = req.header("Content-Type", t);
            }
            match req.send(bytes) {
                Ok(r) if r.status().is_success() => {
                    let etag = r.headers().get("etag").and_then(|v| v.to_str().ok());
                    return etag.map(str::to_owned).ok_or_else(|| "the storage didn't confirm a part".into());
                }
                // Signed for exactly this; asking again won't change it.
                Ok(r) if r.status() == 403 => return Err(format!("the storage refused the upload ({})", r.status())),
                Ok(r) => last = format!("the storage answered {}", r.status()),
                Err(e) => last = offline(e),
            }
        }
        Err(last)
    }
}

/// Where the token is kept. Another server (`HASTECLIP_API`) gets a token
/// file of its own, so trying one out never touches the real token.
fn token_path() -> Option<PathBuf> {
    let name = if std::env::var_os("HASTECLIP_API").is_some() { "share-dev.json" } else { "share.json" };
    dirs::config_dir().map(|d| d.join("hasteclip").join(name))
}

/// The server's own words for what went wrong (`{"error": "…"}`).
fn error_of(res: &mut ureq::http::Response<ureq::Body>) -> String {
    let status = res.status();
    #[derive(Deserialize)]
    struct Error {
        error: String,
    }
    match res.body_mut().read_json::<Error>() {
        Ok(e) => e.error.trim_end_matches('.').to_owned(),
        Err(_) => format!("the server answered {status}"),
    }
}

/// The server can't be reached (no connection, or it's down).
const OFFLINE: &str = "HasteClip's server can't be reached. Check your connection";
/// The server doesn't have the clip: unwatched for 90 days, or taken down.
const NOT_THERE: &str = "it isn't on HasteClip's server anymore";

fn offline(e: ureq::Error) -> String {
    match e {
        ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(_) => OFFLINE.into(),
        e => e.to_string(),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A link said to be gone ([`NOT_THERE`]) is only taken to be once the
/// list of what's shared says so too: a server that doesn't know a request
/// (an older one) answers it the same way, and the link mustn't be
/// forgotten over that.
fn confirm_gone<T>(link: &Link, result: Result<T, String>) -> Result<T, String> {
    match result {
        Err(e) if e == NOT_THERE && status(link) != Some(Status::Gone) => Err("the server couldn't take it".into()),
        r => r,
    }
}

/// How long ago `at` ([`now`]'s seconds) was.
fn now_secs_since(at: u64) -> Duration {
    Duration::from_secs(now().saturating_sub(at))
}

/// A folder for the web copy and its picture, gone when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("hasteclip-share-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a link job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Doing {
    /// Uploading the clip for a link.
    Sharing,
    /// Uploading a new version of it under its link.
    Updating,
    /// Renaming its page.
    Renaming,
    /// Stopping its link: Stop sharing, or the clip was deleted.
    Stopping,
}

/// Something running for a clip's link: one at a time per clip.
pub struct Job {
    clip: PathBuf,
    doing: Doing,
    /// The link's id, when it has one.
    id: Option<String>,
    /// Started by itself, keeping a link up to date (not by a click): it
    /// says nothing if it fails, and tries again later.
    quiet: bool,
    progress: Arc<Progress>,
    done: Receiver<Result<Done, String>>,
}

enum Done {
    Shared(Link),
    /// Brought up to date: the link is this now.
    Updated(Link),
    Unshared,
}

/// What a clip's card shows of its link.
pub(crate) enum LinkState {
    /// Uploading for a link, this far (0..=1).
    Uploading(f32),
    /// Uploading a new version under its link, this far.
    Updating(f32),
    /// Its link shows an older version or name of it, until it can be
    /// brought up to date.
    Behind,
    Shared,
}

/// How often shared clips are looked at for changes.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// How long to wait before trying again to bring a link up to date, after
/// `tries` that didn't work: half a minute, doubling, at most half an hour.
fn backoff(tries: u32) -> Duration {
    Duration::from_secs(30 << tries.min(6)).min(Duration::from_secs(30 * 60))
}

/// The server being asked whether a clip's link still works.
pub struct Check {
    clip: PathBuf,
    done: Receiver<Option<Status>>,
}

/// The Share with a link dialog, for one clip.
pub struct Dialog {
    clip: PathBuf,
    /// Why the last try didn't work.
    error: Option<String>,
    /// Its old link had stopped working.
    note: Option<String>,
    /// How many times its page was opened, once the server says.
    views: Option<i64>,
    /// When its link was copied, to say so on the button for a moment.
    copied: Option<Instant>,
}

/// What was clicked in the dialog.
enum Pick {
    Upload,
    CancelUpload,
    Copy,
    Stop,
    /// Bring its link up to date now.
    TryNow,
    Close,
}

const GONE: &str = "Its link had stopped working: nobody watched it for 90 days, or it was taken down";

impl App {
    /// Open the Share with a link dialog for `clip`.
    pub(crate) fn open_link_dialog(&mut self, clip: PathBuf) {
        self.check_link(&clip);
        self.link_dialog = Some(Dialog { clip, error: None, note: None, views: None, copied: None });
    }

    /// Copy the link of a clip that's shared.
    pub(crate) fn copy_link(&mut self, clip: &Path) {
        let Some(link) = self.links.get(clip) else { return };
        self.ctx().copy_text(link.url.clone());
        self.toast_icon(icon::LINK, format!("Link copied: paste it into Discord or anywhere ({})", crate::hotkey_label_cmd("V")));
        // Said if it turns out to have stopped working.
        self.check_link(clip);
    }

    /// What the clip's card shows of its link, if it has one (or one's on
    /// its way).
    pub(crate) fn link_state(&self, clip: &Path) -> Option<LinkState> {
        match self.link_jobs.iter().find(|j| j.clip == clip) {
            Some(j) if j.doing == Doing::Sharing => return Some(LinkState::Uploading(j.progress.fraction())),
            Some(j) if j.doing == Doing::Updating => return Some(LinkState::Updating(j.progress.fraction())),
            _ => {}
        }
        self.links.get(clip)?;
        Some(if self.links.behind(clip) { LinkState::Behind } else { LinkState::Shared })
    }

    /// A clip was renamed or moved: its link goes with it, and so does
    /// anything on its way.
    pub(crate) fn follow_links(&mut self, from: &Path, to: &Path) {
        self.links.follow(from, to);
        let jobs = self.link_jobs.iter_mut().map(|j| &mut j.clip);
        let checks = self.link_checks.iter_mut().map(|c| &mut c.clip);
        for clip in jobs.chain(checks).chain(self.link_dialog.iter_mut().map(|d| &mut d.clip)) {
            if clip == from {
                *clip = to.to_path_buf();
            }
        }
    }

    /// Ask the server whether the clip's link still works, and how many
    /// times it was watched.
    fn check_link(&mut self, clip: &Path) {
        let Some(link) = self.links.get(clip).cloned() else { return };
        if self.link_checks.iter().any(|c| c.clip == clip) {
            return;
        }
        let (tx, done) = std::sync::mpsc::channel();
        let ctx = self.ctx();
        std::thread::spawn(move || {
            let _ = tx.send(status(&link));
            ctx.request_repaint();
        });
        self.link_checks.push(Check { clip: clip.to_path_buf(), done });
    }

    /// What a clip's page should call it: its name and game, as now.
    fn named(&self, clip: &Path) -> Named {
        let found = self.clips.iter().find(|c| c.path == clip);
        Named { title: found.map(|c| c.title()).unwrap_or_default(), game: found.and_then(|c| c.folder.clone()) }
    }

    /// Upload the clip for a link.
    fn start_share(&mut self, clip: PathBuf) {
        if self.link_jobs.iter().any(|j| j.clip == clip) {
            return;
        }
        let named = self.named(&clip);
        let file = clip.clone();
        self.start_link_job(clip, Doing::Sharing, None, false, move |progress| share(&file, &named, progress).map(Done::Shared));
    }

    /// Delete the clip from the server; its link stops working.
    fn stop_sharing(&mut self, clip: PathBuf) {
        let Some(link) = self.links.get(&clip).cloned() else { return };
        // Bringing it up to date is moot now.
        if let Some(i) = self.link_jobs.iter().position(|j| j.clip == clip && j.quiet) {
            self.link_jobs.remove(i).progress.cancel.store(true, Ordering::Relaxed);
        }
        if self.link_jobs.iter().any(|j| j.clip == clip) {
            return;
        }
        let id = Some(link.id.clone());
        self.start_link_job(clip, Doing::Stopping, id, false, move |_| unshare(&link).map(|()| Done::Unshared));
    }

    fn start_link_job(&mut self, clip: PathBuf, doing: Doing, id: Option<String>, quiet: bool, work: impl FnOnce(&Progress) -> Result<Done, String> + Send + 'static) {
        let progress = Arc::new(Progress::default());
        let (tx, done) = std::sync::mpsc::channel();
        let (p, ctx) = (progress.clone(), self.ctx());
        std::thread::spawn(move || {
            let _ = tx.send(work(&p));
            ctx.request_repaint();
        });
        self.link_jobs.push(Job { clip, doing, id, quiet, progress, done });
    }

    /// Every couple of seconds: links that don't show their clip as it is
    /// now are brought up to date (a new name, or a new version uploaded),
    /// and the links of clips deleted a while ago stop. What can't be done
    /// now (no connection) is tried again later.
    fn sync_links(&mut self) {
        if self.links.looked.is_some_and(|t| t.elapsed() < LOOK_EVERY) {
            return;
        }
        self.links.looked = Some(Instant::now());
        let now = Instant::now();
        let busy = |app: &App, clip: &Path, id: &str| app.link_jobs.iter().any(|j| j.clip == clip) || app.links.retry.get(id).is_some_and(|(at, _)| *at > now);
        let due: Vec<Deleted> = self.links.deleted.iter().filter(|d| now_secs_since(d.at) >= UNDO_FOR).cloned().collect();
        for d in due {
            let clip = self.links.lib.join(&d.key);
            if busy(self, &clip, &d.link.id) {
                continue;
            }
            let id = Some(d.link.id.clone());
            self.start_link_job(clip, Doing::Stopping, id, true, move |_| unshare(&d.link).map(|()| Done::Unshared));
        }
        let mut behind = std::collections::HashSet::new();
        for (clip, link) in self.links.all() {
            // Gone from here some other way (renamed outside HasteClip, say):
            // its link stays as it is. Being saved: once it's done.
            if !self.clips.iter().any(|c| c.path == clip) || self.renders.iter().any(|j| !j.as_new && j.source == clip) {
                continue;
            }
            let named = self.named(&clip);
            let version = Version::of(&clip);
            // A link kept before they were known: taken to be up to date.
            let (Some(had_named), Some(had_version)) = (&link.named, link.version) else {
                self.links.set(&clip, Link { named: Some(named), version, ..link });
                continue;
            };
            let new_file = Some(had_version) != version;
            if !new_file && *had_named == named {
                continue;
            }
            if let Some(key) = self.links.key(&clip) {
                behind.insert(key);
            }
            if busy(self, &clip, &link.id) {
                continue;
            }
            let id = Some(link.id.clone());
            if new_file {
                let file = clip.clone();
                self.start_link_job(clip, Doing::Updating, id, true, move |p| confirm_gone(&link, replace(&file, &link, &named, p)).map(Done::Updated));
            } else {
                self.start_link_job(clip, Doing::Renaming, id, true, move |_| confirm_gone(&link, rename(&link, &named)).map(|()| Done::Updated(Link { named: Some(named), ..link })));
            }
        }
        self.links.behind = behind;
    }

    /// Each frame: how the uploads, unshares and checks went, once done.
    pub(crate) fn pump_links(&mut self) {
        self.sync_links();
        if !self.link_jobs.is_empty() {
            // Their progress, on the clips and in the dialog.
            self.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let mut i = 0;
        while i < self.link_jobs.len() {
            let result = match self.link_jobs[i].done.try_recv() {
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("it stopped unexpectedly".to_owned()),
                Ok(result) => result,
            };
            let job = self.link_jobs.remove(i);
            self.link_job_done(job, result);
        }
        let mut i = 0;
        while i < self.link_checks.len() {
            let status = match self.link_checks[i].done.try_recv() {
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => None,
                Ok(status) => status,
            };
            let check = self.link_checks.remove(i);
            self.link_checked(&check.clip, status);
        }
    }

    fn link_job_done(&mut self, job: Job, result: Result<Done, String>) {
        let shown = self.link_dialog.as_ref().is_some_and(|d| d.clip == job.clip);
        let title = self.clips.iter().find(|c| c.path == job.clip).map(|c| c.title()).unwrap_or_default();
        // A deleted clip's link, stopped (or already gone): it's forgotten.
        if job.quiet && job.doing == Doing::Stopping {
            match result {
                Ok(_) => self.links.forget_deleted(job.id.as_deref().unwrap_or_default()),
                Err(_) => self.links.failed(job.id.as_deref().unwrap_or_default()),
            }
            return;
        }
        let ours = |links: &Links| links.get(&job.clip).is_some_and(|l| Some(&l.id) == job.id.as_ref());
        match result {
            // Kept, unless the link was stopped (or shared anew) meanwhile.
            Ok(Done::Updated(link)) => {
                if ours(&self.links) {
                    self.links.set(&job.clip, link);
                }
            }
            Err(e) if e == NOT_THERE && job.quiet => {
                if ours(&self.links) {
                    self.links.remove(&job.clip);
                    match self.link_dialog.as_mut().filter(|_| shown) {
                        Some(d) => d.note = Some(format!("{GONE}. Upload it again for a new link.")),
                        None => self.toast_error(format!("{title}: {}. Share it again for a new one", GONE.to_lowercase())),
                    }
                }
            }
            Err(_) if job.quiet => self.links.failed(job.id.as_deref().unwrap_or_default()),
            Ok(Done::Shared(link)) => {
                self.links.set(&job.clip, link);
                match self.link_dialog.as_mut().filter(|_| shown) {
                    Some(d) => d.views = Some(0),
                    // Ready while the dialog's closed: a button to copy it,
                    // never straight onto the clipboard (it may hold
                    // something else by now).
                    None => self.toast_action(icon::LINK, format!("{title} is shared"), "Copy link", crate::shell::ToastAction::CopyLink(job.clip)),
                }
            }
            Ok(Done::Unshared) => {
                self.links.remove(&job.clip);
                if shown {
                    self.link_dialog = None;
                }
                self.toast_icon(icon::LINK_BREAK, "Stopped sharing: the link doesn't work anymore");
            }
            // Called off: nothing to say.
            Err(_) if job.progress.cancel.load(Ordering::Relaxed) => {}
            Err(e) => {
                let sharing = job.doing == Doing::Sharing;
                match self.link_dialog.as_mut().filter(|_| shown) {
                    Some(d) => d.error = Some(if sharing { format!("Couldn't upload it: {e}") } else { format!("Couldn't stop sharing: {e}") }),
                    None if sharing => self.toast_error(format!("Couldn't share {title}: {e}")),
                    None => self.toast_error(format!("Couldn't stop sharing {title}: {e}")),
                }
            }
        }
    }

    fn link_checked(&mut self, clip: &Path, status: Option<Status>) {
        let dialog = self.link_dialog.as_mut().filter(|d| d.clip == clip);
        match status {
            Some(Status::Shared { views }) => {
                if let Some(d) = dialog {
                    d.views = Some(views);
                }
            }
            Some(Status::Gone) => {
                self.links.remove(clip);
                match dialog {
                    Some(d) => d.note = Some(format!("{GONE}. Upload it again for a new link.")),
                    None => self.toast_error(format!("{GONE}. Share it again for a new one")),
                }
            }
            // The server can't be asked: the link stays as it is.
            None => {}
        }
    }

    /// "Share with a link": what it does, and Upload. Then how far the upload
    /// is (it goes on with the dialog closed), then the link to copy, how
    /// many have watched it, and Stop sharing.
    pub(crate) fn link_dialog(&mut self, ctx: &egui::Context) {
        let Some(clip) = self.link_dialog.as_ref().map(|d| d.clip.clone()) else { return };
        // Its clip is gone (deleted, or another library): nothing to show.
        let Some(found) = self.clips.iter().find(|c| c.path == clip).cloned() else {
            self.link_dialog = None;
            return;
        };
        let tex = self.thumbs.get(ctx, &found).and_then(|t| t.texture.clone());
        let job = self.link_jobs.iter().find(|j| j.clip == clip);
        let sent = |j: &Job| (j.progress.sent.load(Ordering::Relaxed), j.progress.total.load(Ordering::Relaxed));
        let uploading = job.filter(|j| j.doing == Doing::Sharing).map(sent);
        let updating = job.filter(|j| j.doing == Doing::Updating).map(sent);
        let stopping = job.is_some_and(|j| j.doing == Doing::Stopping);
        let link = self.links.get(&clip).cloned();
        let behind = updating.is_none() && self.links.behind(&clip);
        let Some(d) = &self.link_dialog else { return };
        let (error, note, views) = (d.error.clone(), d.note.clone(), d.views);
        let copied = d.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
        if copied {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let under = format!("{} · {}", found.folder.as_deref().unwrap_or("Other"), found.human_size());
        let mut pick = None;
        let modal = ui_kit::dialog(ctx, "share_link", 420.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, if link.is_some() { "Shared with a link" } else { "Share with a link" });
            crate::library::clip_row(ui, &found, tex.as_ref(), Some(&under));
            ui.add_space(12.0);
            if stopping {
                ui_kit::dialog_text(ui, "Stopping sharing…");
            } else if let Some(link) = &link {
                if link_row(ui, &link.url, copied) {
                    pick = Some(Pick::Copy);
                }
                ui.add_space(8.0);
                let watched = match views {
                    Some(0) => "Not watched yet. ".to_owned(),
                    Some(1) => "Watched once. ".to_owned(),
                    Some(n) => format!("Watched {n} times. "),
                    None => String::new(),
                };
                ui_kit::dialog_text(ui, &format!("{watched}Anyone with the link can watch it, in a browser or right in Discord. A link nobody watches for 90 days stops working."));
                // It follows the clip: a new version on its way, or waiting
                // to be.
                if let Some((sent, total)) = updating {
                    ui.add_space(10.0);
                    progress_bar(ui, sent as f32 / total.max(1) as f32);
                    ui.add_space(8.0);
                    let how = match (sent * 100).checked_div(total) {
                        None => "Getting the new version ready…".to_owned(),
                        Some(pct) => format!("Updating the link to this version… {}%", pct.min(99)),
                    };
                    ui.label(theme::text(how, 13.5, W::Medium).color(p.text));
                    ui.add_space(4.0);
                    ui_kit::dialog_text(ui, "Until it's all there, the link plays the version from before.");
                } else if behind {
                    ui.add_space(8.0);
                    ui_kit::dialog_text(ui, "The link still shows it as it was before. It's brought up to date by itself as soon as HasteClip's server can be reached.");
                }
            } else if let Some((sent, total)) = uploading {
                progress_bar(ui, sent as f32 / total.max(1) as f32);
                ui.add_space(8.0);
                // No total yet: the copy to upload is still being made.
                let how = match (sent * 100).checked_div(total) {
                    None => "Getting it ready…".to_owned(),
                    Some(pct) => format!("Uploading… {}% · {} of {}", pct.min(99), crate::export_ui::human_bytes(sent), crate::export_ui::human_bytes(total)),
                };
                ui.label(theme::text(how, 13.5, W::Medium).color(p.text));
                ui.add_space(4.0);
                ui_kit::dialog_text(ui, "You can close this: it goes on uploading, and the clip shows how far it's come. You'll be told when its link is ready.");
            } else {
                if let Some(n) = &note {
                    ui_kit::dialog_text(ui, n);
                }
                ui_kit::dialog_text(ui, "Uploads the clip to HasteClip for a link that plays it in any browser, and right in Discord. Anyone with the link can watch it, until you stop sharing.");
            }
            if let Some(e) = &error {
                ui.add_space(4.0);
                ui.add(egui::Label::new(theme::text(e, 12.5, W::Regular).color(p.danger)).wrap());
            }
            ui_kit::dialog_buttons(ui, |ui| {
                if stopping {
                    if ui.add(ui_kit::button("Close", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                } else if link.is_some() {
                    if ui.add(ui_kit::button("Done", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                    if behind && ui.add(ui_kit::button("Try now", false).ghost()).clicked() {
                        pick = Some(Pick::TryNow);
                    }
                    if Btn::new(Kind::DangerText, "Stop sharing").tip("Deletes it from HasteClip: the link stops working for everyone").show(ui).clicked() {
                        pick = Some(Pick::Stop);
                    }
                } else if uploading.is_some() {
                    if ui.add(ui_kit::button("Close", false)).clicked() {
                        pick = Some(Pick::Close);
                    }
                    if ui.add(ui_kit::button("Cancel upload", false).ghost()).clicked() {
                        pick = Some(Pick::CancelUpload);
                    }
                } else {
                    if Btn::primary(if error.is_some() { "Try again" } else { "Upload" }).with_icon(icon::CLOUD_ARROW_UP).show(ui).clicked() {
                        pick = Some(Pick::Upload);
                    }
                    if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
                        pick = Some(Pick::Close);
                    }
                }
            });
            // Enter does the main thing: upload, or copy the link.
            if !stopping && uploading.is_none() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                pick = Some(if link.is_some() { Pick::Copy } else { Pick::Upload });
            }
        });
        match pick {
            Some(Pick::Upload) => {
                if let Some(d) = &mut self.link_dialog {
                    d.error = None;
                    d.note = None;
                }
                self.start_share(clip);
            }
            Some(Pick::CancelUpload) => {
                // Let go of it now; it stops (and its half on the server goes)
                // at the next part.
                if let Some(i) = self.link_jobs.iter().position(|j| j.doing == Doing::Sharing && j.clip == clip) {
                    self.link_jobs.remove(i).progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            Some(Pick::TryNow) => self.links.try_now(&clip),
            Some(Pick::Copy) => {
                if let Some(link) = &link {
                    ctx.copy_text(link.url.clone());
                }
                if let Some(d) = &mut self.link_dialog {
                    d.copied = Some(Instant::now());
                }
            }
            Some(Pick::Stop) => {
                if let Some(d) = &mut self.link_dialog {
                    d.error = None;
                }
                self.stop_sharing(clip);
            }
            Some(Pick::Close) => self.link_dialog = None,
            None if modal.dismissed() => self.link_dialog = None,
            None => {}
        }
    }
}

/// The link, and a Copy button beside it. True when Copy was clicked.
fn link_row(ui: &mut egui::Ui, url: &str, copied: bool) -> bool {
    let p = theme::pal(ui);
    const COPY_W: f32 = 96.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        egui::Frame::new().fill(p.control).corner_radius(ui_kit::RADIUS).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
            ui.set_width(ui.available_width() - COPY_W - 8.0);
            ui.add(egui::Label::new(theme::text(url, 13.5, W::Regular).color(p.text)).truncate());
        });
        Btn::primary(if copied { "Copied" } else { "Copy" }).with_icon(if copied { icon::CHECK } else { icon::COPY }).min_width(COPY_W).show(ui).clicked()
    })
    .inner
}

/// A bar filled `f` (0..=1) of the way.
fn progress_bar(ui: &mut egui::Ui, f: f32) {
    let p = theme::pal(ui);
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 3, p.control);
    ui.painter().rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.left() + r.width() * f.clamp(0.0, 1.0), r.bottom())), 3, p.text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_follow_their_clip_and_are_saved() {
        let lib = std::env::temp_dir().join(format!("hc-links-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let link = Link { id: "aB3xY9k2Qp".into(), url: "https://clips.hasteclip.com/aB3xY9k2Qp".into(), shared: 1, named: None, version: None };
        let mut links = Links::load(&lib);
        links.set(&lib.join("Game").join("Ace.mp4"), link.clone());
        links.follow(&lib.join("Game").join("Ace.mp4"), &lib.join("Ace!.mp4"));
        let again = Links::load(&lib);
        assert_eq!(again.get(&lib.join("Ace!.mp4")), Some(&link));
        assert_eq!(again.get(&lib.join("Game").join("Ace.mp4")), None);
        let mut again = again;
        again.remove(&lib.join("Ace!.mp4"));
        assert_eq!(Links::load(&lib).get(&lib.join("Ace!.mp4")), None);
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// A deleted clip's link waits for Undo (kept across a restart), and
    /// Undo brings it back.
    #[test]
    fn deleted_links_wait_for_undo() {
        let lib = std::env::temp_dir().join(format!("hc-links-gone-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let link = Link { id: "aB3xY9k2Qp".into(), url: "u".into(), shared: 1, named: None, version: None };
        let clip = lib.join("Game").join("Ace.mp4");
        let mut links = Links::load(&lib);
        links.set(&clip, link.clone());
        links.deleted(&clip);
        assert_eq!(links.get(&clip), None);
        let mut again = Links::load(&lib);
        assert_eq!(again.deleted.len(), 1, "it waits, even after a restart");
        again.undeleted(&clip);
        assert_eq!(again.get(&clip), Some(&link));
        assert!(Links::load(&lib).deleted.is_empty());
        // Gone for good once stopped.
        again.deleted(&clip);
        again.forget_deleted(&link.id);
        let last = Links::load(&lib);
        assert!(last.deleted.is_empty() && last.get(&clip).is_none());
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// An edit is a new file; a rename isn't.
    #[test]
    fn versions_tell_edits_from_renames() {
        let dir = std::env::temp_dir().join(format!("hc-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Ace.mp4");
        std::fs::write(&clip, b"the recording").unwrap();
        let first = Version::of(&clip).unwrap();
        let renamed = dir.join("Ace clutch.mp4");
        std::fs::rename(&clip, &renamed).unwrap();
        assert_eq!(Version::of(&renamed), Some(first));
        // Saved as an edit is: written aside, renamed over it.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("staged.mp4"), b"the edit").unwrap();
        std::fs::rename(dir.join("staged.mp4"), &renamed).unwrap();
        assert_ne!(Version::of(&renamed), Some(first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retries_wait_longer_each_time() {
        assert_eq!(backoff(0), Duration::from_secs(30));
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(20), Duration::from_secs(30 * 60));
    }

    #[test]
    fn the_web_copy_is_a_faststart_mp4_with_the_mix_and_no_tags() {
        let dir = std::env::temp_dir().join(format!("hc-webcopy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Clip.mp4");
        // Two sound tracks (the mix, then a source) and a tag, like a clip.
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=black:s=64x64:d=2", "-f", "lavfi", "-i", "sine=f=440:d=2", "-f", "lavfi", "-i", "sine=f=880:d=2"])
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-c:a", "aac", "-metadata", "comment=hasteclip:id=secret", "-shortest"])
            .arg(&clip)
            .status()
            .unwrap();
        if !made.success() {
            // An ffmpeg without x264 (an LGPL build) can't make the test clip.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let out = dir.join("web.mp4");
        web_copy(&clip, &out).unwrap();
        let probe = media::ffprobe()
            .args(["-v", "error", "-show_entries", "stream=codec_type:format_tags=comment", "-of", "json"])
            .arg(&out)
            .output()
            .unwrap();
        let json = String::from_utf8_lossy(&probe.stdout).into_owned();
        // moov before mdat: it can play before it's all downloaded.
        let bytes = std::fs::read(&out).unwrap();
        let at = |tag: &[u8]| bytes.windows(4).position(|w| w == tag).unwrap();
        let (moov, mdat) = (at(b"moov"), at(b"mdat"));
        assert!(picture(&out, 2.0, &dir.join("thumb.jpg")));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(json.matches("\"audio\"").count(), 1, "{json}");
        assert!(!json.contains("secret"), "{json}");
        assert!(moov < mdat);
    }

    /// Share a clip for real, watch it, rename it, replace it with a shorter
    /// version, stop sharing it.
    #[test]
    #[ignore = "uploads to a real server: HASTECLIP_API=http://localhost:8080 cargo test -p hasteclip share_end_to_end -- --ignored"]
    fn share_end_to_end() {
        assert!(std::env::var_os("HASTECLIP_API").is_some(), "point HASTECLIP_API at a server that isn't the real one");
        let dir = std::env::temp_dir().join(format!("hc-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("Clip.mp4");
        // Noise at a high bitrate: ~20 MB, a few parts to upload.
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "nullsrc=s=1280x720:r=30,geq=random(1)*255:128:128", "-f", "lavfi", "-i", "sine=f=440"])
            .args(["-t", "4", "-c:v", "libx264", "-b:v", "40M", "-c:a", "aac", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let progress = Progress::default();
        let named = Named { title: "End to end".into(), game: Some("Test Game".into()) };
        let link = share(&clip, &named, &progress).unwrap();
        assert!(link.url.ends_with(&link.id), "{link:?}");
        assert_eq!((link.named.as_ref(), link.version), (Some(&named), Version::of(&clip)));
        assert_eq!(progress.sent.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
        assert_eq!(status(&link), Some(Status::Shared { views: 0 }));

        // The video comes from the bucket, by way of the link.
        let mut res = ureq::get(format!("{}.mp4", link.url)).header("Range", "bytes=0-11").call().unwrap();
        let start = res.body_mut().read_to_vec().unwrap();
        assert_eq!(&start[4..8], b"ftyp");
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains("og:video") && page.contains("End to end") && page.contains("Test Game"));
        assert!(page.contains(".mp4?v=1"), "files linked by version");

        // Renamed, and moved to another game.
        let renamed = Named { title: "Renamed".into(), game: None };
        rename(&link, &renamed).unwrap();
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains("Renamed") && !page.contains("Test Game"), "{page}");

        // A shorter version, under the same link.
        let size = |url: &str| -> u64 {
            let res = ureq::get(url).header("Range", "bytes=0-0").call().unwrap();
            let range = res.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            range.rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0)
        };
        let before = size(&format!("{}.mp4", link.url));
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&clip)
            .args(["-t", "1.5", "-c", "copy"])
            .arg(dir.join("Trimmed.mp4"))
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::rename(dir.join("Trimmed.mp4"), &clip).unwrap();
        let progress = Progress::default();
        let replaced = replace(&clip, &link, &renamed, &progress).unwrap();
        assert_eq!((replaced.id.as_str(), replaced.url.as_str()), (link.id.as_str(), link.url.as_str()), "the same link");
        assert_eq!(replaced.version, Version::of(&clip));
        let after = size(&format!("{}.mp4?v=2", link.url));
        assert!(after > 0 && after < before, "the new version plays: {after} < {before}");
        let page = ureq::get(&link.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert!(page.contains(".mp4?v=2"), "the page links the new version");

        unshare(&link).unwrap();
        assert_eq!(status(&link), Some(Status::Gone));
        assert_eq!(ureq::get(&link.url).config().http_status_as_error(false).build().call().unwrap().status(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
