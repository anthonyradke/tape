//! "Copy link": makes a smaller copy of a clip in a process of its own, uploads it to the share
//! server in parts, and puts the link on the clipboard. Also keeps the server's storage numbers,
//! which the UI shows against the free allowance. The server is `share/worker.js`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, inside_folder};
use crate::state::Shared;
use crate::supervisor;

const PART_TRIES: u32 = 3;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub id: String,
    pub url: String,
    pub name: String,
    /// The local clip's key, so the gallery can mark it and reuse the link.
    pub source: String,
    pub bytes: u64,
    pub seconds: f64,
    pub uploaded: u64,
    pub expires: u64,
}

/// What the share server holds, from `GET /api/usage`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub used: u64,
    /// The free allowance, 10 GB on R2.
    pub free: u64,
    /// Where the server stops taking uploads, a little under `free`.
    pub cap: u64,
    pub link_days: u32,
    pub clips: Vec<Link>,
}

/// One clip on its way up, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub path: String,
    /// "converting", "uploading", "done" or "failed".
    pub stage: &'static str,
    pub progress: f64,
    pub url: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Shares {
    jobs: Mutex<HashMap<String, Job>>,
    usage: Mutex<Option<Usage>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    child: Mutex<Option<Child>>,
}

struct Server {
    url: String,
    key: String,
}

fn server(app: &AppHandle) -> Result<Server, String> {
    let settings = app.state::<Shared>().settings.lock().unwrap().clone();
    if settings.share_server.is_empty() || settings.share_key.is_empty() {
        return Err("Sharing isn't set up yet. Add the share server and upload key in Settings.".into());
    }
    Ok(Server { url: settings.share_server, key: settings.share_key })
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        reqwest::Client::builder()
            .user_agent(concat!("tape/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("an HTTP client")
    })
}

/// A request to the server's API, with its error turned into words for the UI.
async fn call(server: &Server, method: reqwest::Method, path: &str, body: Option<reqwest::Body>, timeout: Duration) -> Result<Value, String> {
    let mut request = client().request(method, format!("{}{path}", server.url)).bearer_auth(&server.key).timeout(timeout);
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send().await.map_err(|e| format!("Couldn't reach the share server: {}", plain(&e)))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("The share server turned down the upload key. Check it in Settings.".into());
    }
    if !status.is_success() {
        let said = value.get("error").and_then(Value::as_str).map(str::to_string);
        return Err(said.unwrap_or_else(|| format!("The share server answered {status}.")));
    }
    Ok(value)
}

/// reqwest's errors read as a chain; the innermost cause is the useful part.
fn plain(error: &reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = error;
    while let Some(next) = source.source() {
        source = next;
    }
    source.to_string()
}

async fn fetch_usage(app: &AppHandle) -> Result<Usage, String> {
    let server = server(app)?;
    let value = call(&server, reqwest::Method::GET, "/api/usage", None, Duration::from_secs(20)).await?;
    let usage: Usage = serde_json::from_value(value).map_err(|e| format!("The share server sent something unexpected: {e}"))?;
    *app.state::<Shares>().usage.lock().unwrap() = Some(usage.clone());
    let _ = app.emit("share-usage", &usage);
    Ok(usage)
}

fn set_job(app: &AppHandle, job: Job) {
    app.state::<Shares>().jobs.lock().unwrap().insert(job.path.clone(), job.clone());
    let _ = app.emit("share", job);
}

fn job(path: &str, stage: &'static str, progress: f64) -> Job {
    Job { path: path.to_string(), stage, progress, url: None, error: None }
}

/// Runs `tape --share-copy` and passes its progress on. Killed if the share is cancelled.
fn convert(app: &AppHandle, input: &Path, output: &Path) -> Result<(u32, u32, f64), String> {
    let shared = app.state::<Shared>();
    let mut process = supervisor::own_process().map_err(|e| e.to_string())?;
    process.arg("--share-copy").arg(input).arg(output).arg(&shared.log_path);
    let mut child = process.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("Couldn't start the conversion: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    *app.state::<Shares>().child.lock().unwrap() = Some(child);
    let path = input.display().to_string();
    let mut result = Err("The conversion stopped without finishing.".to_string());
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(p) = value.get("progress").and_then(Value::as_f64) {
            set_job(app, job(&path, "converting", p));
        } else if let Some(done) = value.get("done") {
            let number = |k: &str| done.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            result = Ok((number("width") as u32, number("height") as u32, number("seconds")));
        } else if let Some(error) = value.get("error").and_then(Value::as_str) {
            result = Err(format!("Couldn't convert the clip: {error}"));
        }
    }
    if let Some(mut child) = app.state::<Shares>().child.lock().unwrap().take() {
        let _ = child.wait();
    }
    result
}

/// Sends the file in the parts the server asked for, retrying each a few times.
async fn upload(app: &AppHandle, server: &Server, path: &str, file: &Path, start: &Value, cancel: &AtomicBool) -> Result<(), String> {
    let id = start["id"].as_str().unwrap_or_default();
    let upload = start["upload"].as_str().unwrap_or_default();
    let part_bytes = start["partBytes"].as_u64().unwrap_or(20 << 20) as usize;
    let total = std::fs::metadata(file).map_err(|e| e.to_string())?.len().max(1);
    let mut reader = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut parts = Vec::new();
    let mut sent = 0u64;
    for number in 1.. {
        let mut chunk = Vec::with_capacity(part_bytes);
        (&mut reader).take(part_bytes as u64).read_to_end(&mut chunk).map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            break;
        }
        let size = chunk.len() as u64;
        let mut last = String::new();
        let mut done = None;
        for _ in 0..PART_TRIES {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled.".into());
            }
            let route = format!("/api/clips/{id}/parts/{number}?upload={upload}");
            match call(server, reqwest::Method::PUT, &route, Some(chunk.clone().into()), Duration::from_secs(600)).await {
                Ok(v) => {
                    done = Some(v);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let Some(part) = done else { return Err(last) };
        parts.push(json!({ "part": part["part"], "etag": part["etag"] }));
        sent += size;
        set_job(app, job(path, "uploading", sent as f64 / total as f64));
    }
    let route = format!("/api/clips/{id}/complete?upload={upload}");
    call(server, reqwest::Method::POST, &route, Some(json!({ "parts": parts }).to_string().into()), Duration::from_secs(60)).await?;
    Ok(())
}

async fn share(app: &AppHandle, path: &str, cancel: &AtomicBool) -> Result<String, String> {
    let server = server(app)?;
    let input = PathBuf::from(path);
    let key = commands::clip_key(&input).ok_or("That clip isn't there any more.")?;

    // Shared before and still live: the same link again, no upload.
    let known = app.state::<Shares>().usage.lock().unwrap().clone();
    let usage = match known {
        Some(u) => u,
        None => fetch_usage(app).await?,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    if let Some(link) = usage.clips.iter().find(|l| l.source == key && l.expires > now + 3_600_000) {
        return Ok(link.url.clone());
    }
    if usage.used >= usage.cap {
        return Err("Cloud storage is full. Delete a shared link or wait for one to expire.".into());
    }

    set_job(app, job(path, "converting", 0.0));
    let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("share");
    let _ = std::fs::create_dir_all(&dir);
    let copy = dir.join(format!("{key}.mp4"));
    let converted = {
        let (app, input, copy) = (app.clone(), input.clone(), copy.clone());
        tauri::async_runtime::spawn_blocking(move || convert(&app, &input, &copy)).await.map_err(|e| e.to_string())?
    };
    let result = async {
        let (width, height, seconds) = converted?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".to_string());
        }
        let bytes = std::fs::metadata(&copy).map_err(|e| e.to_string())?.len();
        let name = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        set_job(app, job(path, "uploading", 0.0));
        let info = json!({ "name": name, "bytes": bytes, "seconds": seconds, "width": width, "height": height, "source": key });
        let start = call(&server, reqwest::Method::POST, "/api/clips", Some(info.to_string().into()), Duration::from_secs(30)).await?;
        let id = start["id"].as_str().unwrap_or_default().to_string();
        if let Err(e) = upload(app, &server, path, &copy, &start, cancel).await {
            let route = format!("/api/clips/{id}?upload={}", start["upload"].as_str().unwrap_or_default());
            let _ = call(&server, reqwest::Method::DELETE, &route, None, Duration::from_secs(20)).await;
            return Err(e);
        }
        // The gallery's thumbnail, for the preview chat apps show. Optional.
        let thumbs = commands::thumbnails(&app.state::<Shared>().thumbnails);
        if let Some(jpeg) = thumbs.get(&key).and_then(|(p, _)| std::fs::read(p).ok()) {
            let _ = call(&server, reqwest::Method::PUT, &format!("/api/clips/{id}/thumb"), Some(jpeg.into()), Duration::from_secs(30)).await;
        }
        Ok(start["url"].as_str().unwrap_or_default().to_string())
    }
    .await;
    let _ = std::fs::remove_file(&copy);
    let _ = fetch_usage(app).await;
    result
}

/// Shares a clip and copies its link. One at a time: conversion uses the GPU beside a game.
#[tauri::command]
pub async fn share_clip(app: AppHandle, path: String) -> Result<String, String> {
    if !inside_folder(&app, &path) {
        return Err("Only clips in the clips folder can be shared.".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let busy = {
        let shares = app.state::<Shares>();
        let mut current = shares.cancel.lock().unwrap();
        let busy = current.is_some();
        if !busy {
            *current = Some(cancel.clone());
        }
        busy
    };
    if busy {
        return Err("Another clip is still being shared. Wait for it to finish.".into());
    }
    let result = share(&app, &path, &cancel).await;
    *app.state::<Shares>().cancel.lock().unwrap() = None;
    match &result {
        Ok(url) => {
            copy_to_clipboard(url);
            set_job(&app, Job { url: Some(url.clone()), ..job(&path, "done", 1.0) });
        }
        Err(e) => set_job(&app, Job { error: Some(e.clone()), ..job(&path, "failed", 0.0) }),
    }
    app.state::<Shares>().jobs.lock().unwrap().remove(&path);
    result
}

#[tauri::command]
pub fn cancel_share(app: AppHandle) {
    let shares = app.state::<Shares>();
    if let Some(flag) = shares.cancel.lock().unwrap().as_ref() {
        flag.store(true, Ordering::Relaxed);
    }
    let mut child = shares.child.lock().unwrap();
    if let Some(child) = child.as_mut() {
        let _ = child.kill();
    }
}

/// Shares in progress, for a window opened partway through one.
#[tauri::command]
pub fn share_jobs(app: AppHandle) -> Vec<Job> {
    app.state::<Shares>().jobs.lock().unwrap().values().cloned().collect()
}

/// The last numbers, or fresh ones. None while sharing isn't set up.
#[tauri::command]
pub async fn share_usage(app: AppHandle, fresh: bool) -> Result<Option<Usage>, String> {
    if server(&app).is_err() {
        return Ok(None);
    }
    let known = app.state::<Shares>().usage.lock().unwrap().clone();
    match known {
        Some(u) if !fresh => Ok(Some(u)),
        _ => fetch_usage(&app).await.map(Some),
    }
}

/// Deletes a link before it expires, which frees its space straight away.
#[tauri::command]
pub async fn unshare(app: AppHandle, id: String) -> Result<Option<Usage>, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Not a link of this server.".into());
    }
    let server = server(&app)?;
    call(&server, reqwest::Method::DELETE, &format!("/api/clips/{id}"), None, Duration::from_secs(20)).await?;
    fetch_usage(&app).await.map(Some)
}

/// Forgets the numbers when the server or key changes, so the next look asks the new one.
pub fn forget(app: &AppHandle) {
    *app.state::<Shares>().usage.lock().unwrap() = None;
}

#[tauri::command]
pub fn copy_text(text: String) {
    copy_to_clipboard(&text);
}

/// From Rust rather than the webview, which may not have focus when an upload finishes.
#[cfg(windows)]
fn copy_to_clipboard(text: &str) {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        // Another app can hold the clipboard for a moment.
        if !(0..10).any(|_| OpenClipboard(None).is_ok() || { std::thread::sleep(Duration::from_millis(20)); false }) {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let target = GlobalLock(memory) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                let _ = GlobalUnlock(memory);
                // The clipboard owns the memory once this succeeds.
                if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).is_err() {
                    let _ = GlobalFree(Some(HGLOBAL(memory.0)));
                }
            }
        }
        let _ = CloseClipboard();
    }
}

#[cfg(not(windows))]
fn copy_to_clipboard(text: &str) {
    // The Linux build is only for tests, which read the link from the job instead.
    let _ = text;
}
