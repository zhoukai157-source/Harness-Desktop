// DSH Desktop — a native macOS shell around the DeepSeek Harness web profile.
//
// The "engine" is the `dsh web` server (a Cordis web app that serves the DSH
// SPA on a local port). This shell:
//   - spawns / attaches to the engine like a normal `dsh web` run
//   - hosts the app lifecycle (tray, menu, single-instance, notifications)
//   - navigates the main window to the engine URL once it is healthy
//
// Security model: the main window starts at our own (bundled) controller page;
// after the engine is healthy we navigate it to the engine origin (loopback).
// We deliberately do NOT enable dangerousRemoteDomainIpcAccess — the remote
// DSH page talks to its own backend over its own protocol, and all desktop
// actions go through Rust (tray/menu), so no remote-origin IPC is needed.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, RunEvent,
};
use tauri_plugin_opener::OpenerExt;

// macOS paste-path bridging (objc2). Used to convert a Finder file copy into
// plain path text when the user pastes with Cmd+V inside the DSH input box.
use block2::RcBlock;
use objc2_app_kit::{
    NSEvent, NSEventMask, NSEventModifierFlags, NSPasteboard, NSPasteboardTypeFileURL,
    NSPasteboardTypeString,
};
use objc2_foundation::{NSString, NSURL};
use std::ptr::NonNull;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The canonical port a terminal `dsh web` uses. If a DSH server already
/// answers here we attach to it instead of spawning a duplicate instance.
const CANONICAL_PORT: u16 = 3080;
/// The port the desktop app owns when it must spawn its own engine.
const DESKTOP_PORT: u16 = 3480;
/// Poll budget when waiting for the engine to come up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_GRACE: Duration = Duration::from_secs(6);

// ---------------------------------------------------------------------------
// Serde payloads shared with the controller page (camelCase over the wire)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct EngineInfo {
    status: String, // starting | ready | attached | error | stopped
    mode: String,   // standalone | attached
    url: Option<String>,
    port: Option<u16>,
    pid: Option<u32>,
    workspace: String,
    detail: Option<String>,
    token_url: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StatusEvent {
    status: String,
    detail: Option<String>,
    url: Option<String>,
}

#[derive(Default)]
struct Engine {
    child: Option<Child>,
    mode: String, // "standalone" | "attached" | "stopped"
    port: Option<u16>,
    url: Option<String>,
    token_url: Option<String>,
    manual_stop: bool,
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct Settings {
    workspace: Option<PathBuf>,
    prefer_port: Option<u16>,
}

impl Settings {
    fn workspace_dir(&self) -> PathBuf {
        self.workspace
            .clone()
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

struct AppState {
    engine: Mutex<Engine>,
    settings: Mutex<Settings>,
    boot_lock: Mutex<()>,      // serializes concurrent start_engine calls
    initial_url: Mutex<String>, // captured at startup: our own controller page
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Timestamped diagnostic logger (seconds since app start). Flushes so logs
/// survive redirects to a file (Rust's stdout is block-buffered otherwise).
fn logf(msg: &str) {
    use std::io::Write;
    println!("[{:.1}s] [dsh-desktop] {msg}", whole().elapsed().as_secs_f64());
    let _ = std::io::stdout().flush();
}
fn whole() -> &'static std::time::Instant {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(|| std::time::Instant::now())
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

fn emit_status(app: &AppHandle, status: &str, detail: Option<String>, url: Option<String>) {
    let _ = app.emit(
        "engine-status",
        StatusEvent {
            status: status.to_string(),
            detail,
            url,
        },
    );
}

fn engine_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn engine_port(state: &AppState) -> u16 {
    state.engine.lock().unwrap().port.unwrap_or(DESKTOP_PORT)
}

/// Obtain the DSH auth cookie's `Name=Value` by performing the token exchange
/// over plain HTTP (GET /?token=… → 303 + Set-Cookie). Returns just the first
/// attribute pair, e.g. `dsh-auth-xxx=v1….JWT`.
///
/// WKWebView drops Set-Cookie from 3xx redirects, so the webview itself can't
/// obtain the cookie; we fetch it here and then write it from JS on the
/// engine's own origin (see action_start).
fn fetch_auth_cookie_name_value(port: u16, token_url: &str) -> Option<String> {
    let token = token_url.split("token=").nth(1)?.split('&').next().unwrap_or("");
    if token.is_empty() { return None; }

    let (status, body) = http_get(port, "/", Some(token))?;
    if status != 303 { logf(&format!("auth exchange: unexpected status {status}")); return None; }

    let body_str = String::from_utf8_lossy(&body);
    for line in body_str.lines() {
        if line.to_lowercase().starts_with("set-cookie:") {
            let value = line.trim_start().strip_prefix("set-cookie:").unwrap_or("").trim();
            if let Some(nv) = value.split(';').next() {
                if !nv.trim().is_empty() {
                    return Some(nv.trim().to_string());
                }
            }
            break;
        }
    }
    None
}

/// Load a URL in the webview by directly calling WKWebView.loadRequest through
/// with_webview. More reliable than tauri's WebviewWindow::navigate (which
/// silently fails to commit the load when called from a background thread).
fn webview_load_url(app: &AppHandle, url: &str) {
    if let Some(win) = app.get_webview_window("main") {
        use objc2_foundation::{NSURL, NSURLRequest, NSString};
        let url_string = url.to_string();
        let _ = win.with_webview(move |webview| {
            unsafe {
                use objc2_web_kit::WKWebView;
                let wv = &*(webview.inner() as *mut WKWebView);
                if let Some(u) = NSURL::URLWithString(&NSString::from_str(&url_string)) {
                    let req = NSURLRequest::requestWithURL(&u);
                    wv.loadRequest(&req);
                    logf(&format!("webview_load_url: loaded {url_string}"));
                }
            }
        });
    }
}

/// Fire-and-forget JS execution on the committed page using the raw
/// WKWebView.evaluateJavaScript path. Logs whether the script executed or
/// threw.
fn eval_fire(app: &AppHandle, label: &str, js: &str) {
    if let Some(win) = app.get_webview_window("main") {
        let script = js.to_string();
        let label = label.to_string();
        use block2::RcBlock;
        let _ = win.with_webview(move |webview| {
            unsafe {
                use objc2::runtime::AnyObject;
                use objc2_foundation::{NSError, NSString};
                use objc2_web_kit::WKWebView;
                let wv = &*(webview.inner() as *mut WKWebView);
                let s = NSString::from_str(&script);
                let block = RcBlock::new(move |_result: *mut AnyObject, err: *mut NSError| {
                    if !err.is_null() {
                        logf(&format!("[eval] {label}: THREW"));
                    } else {
                        logf(&format!("[eval] {label}: ran"));
                    }
                });
                wv.evaluateJavaScript_completionHandler(&s, Some(&*block));
            }
        });
    }
}

/// Diagnostic: read back what the webview is actually displaying (title +
/// first bit of body text) and log it. Lets us verify auth worked without
/// eyeballing the window.
fn diag_page_content(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.with_webview(move |webview| {
            unsafe {
                use block2::RcBlock;
                use objc2::runtime::AnyObject;
                use objc2_foundation::{NSError, NSString};
                use objc2_web_kit::WKWebView;
                let wv = &*(webview.inner() as *mut WKWebView);
                let script = NSString::from_str(
                    "(document.title||'')+'|'+(document.body&&document.body.innerText||'').replace(/\\n/g,' ').slice(0,160)",
                );
                let block = RcBlock::new(move |result: *mut AnyObject, err: *mut NSError| {
                    if !err.is_null() {
                        logf("[diag] evaluateJavaScript error");
                        return;
                    }
                    if result.is_null() {
                        logf("[diag] empty result");
                        return;
                    }
                    let s = &*(result as *const NSString);
                    let text = s.to_string();
                    logf(&format!("[diag] page: {text}"));
                });
                wv.evaluateJavaScript_completionHandler(&script, Some(&*block));
            }
        });
    }
}

fn navigate_main(app: &AppHandle, url: &str) {
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(parsed) = url.parse() {
            let _ = win.navigate(parsed);
        }
    }
}

fn navigate_to_controller(app: &AppHandle) {
    let state = app.state::<AppState>();
    let url = state.initial_url.lock().unwrap().clone();
    navigate_main(app, &url);
}

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

fn snapshot_info(state: &AppState) -> EngineInfo {
    let engine = state.engine.lock().unwrap();
    let settings = state.settings.lock().unwrap();
    EngineInfo {
        status: match (&engine.child, engine.mode.as_str()) {
            (Some(_), "standalone") => "ready".to_string(),
            (None, "attached") => "attached".to_string(),
            _ => "stopped".to_string(),
        },
        mode: engine.mode.clone(),
        url: engine.url.clone(),
        port: engine.port,
        pid: engine.child.as_ref().map(|c| c.id()),
        workspace: settings.workspace_dir().display().to_string(),
        detail: None,
        token_url: engine.token_url.clone(),
    }
}

// ---------------------------------------------------------------------------
// dsh binary discovery
// ---------------------------------------------------------------------------

/// Newest `node_modules/.bin/dsh` under an npm/npx cache root, if any.
fn scan_npx_root(root: &PathBuf) -> Option<PathBuf> {
    let npx_dir = root.join("_npx");
    let read = fs::read_dir(npx_dir).ok()?;
    let mut cands: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let cand = entry.path().join("node_modules/.bin/dsh");
        if cand.is_file() {
            if let Ok(md) = fs::metadata(&cand) {
                if let Ok(t) = md.modified() {
                    cands.push((t, cand));
                }
            }
        }
    }
    cands.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    cands.into_iter().next().map(|(_, bin)| bin)
}

/// Locate the `dsh` CLI the same way a developer would have installed it:
/// first on `PATH`, then in the global npm/npx cache (~/.npm/_npx).
fn find_dsh() -> Option<PathBuf> {
    // 1) On PATH.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("dsh");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 2) The user's global npm/npx cache (~/.npm/_npx), newest first.
    if let Some(home) = home_dir() {
        if let Some(bin) = scan_npx_root(&home.join(".npm")) {
            return Some(bin);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Minimal loopback HTTP probing (no extra dependency)
// ---------------------------------------------------------------------------

/// Synchronous GET `path` on 127.0.0.1:`port`, optionally appending a
/// `?token=xxx` query string. Returns (status, body) on success, `None`
/// when nothing answers or the read fails.
fn http_get(port: u16, path: &str, token: Option<&str>) -> Option<(u16, Vec<u8>)> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse().ok()?, PROBE_TIMEOUT).ok()?;
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let full_path = match token {
        Some(t) if !t.is_empty() => {
            if path.contains('?') {
                format!("{path}&token={t}")
            } else {
                format!("{path}?token={t}")
            }
        }
        _ => path.to_string(),
    };
    let req = format!(
        "GET {full_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut body = Vec::new();
    let mut tmp = [0u8; 8192];
    while body.len() < 512 * 1024 {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    let head = String::from_utf8_lossy(&body);
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body))
}

fn server_up(port: u16) -> bool {
    http_get(port, "/", None)
        .is_some_and(|(s, _)| s == 200 || s == 401)
}

fn looks_like_dsh(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(body);
    head.contains("__DSH_BOOT__") || head.contains("DeepSeek Harness")
}

/// Whether the server on `port` is a DSH instance. Auth-gated servers (401)
/// are accepted without body inspection.
fn is_dsh(port: u16) -> bool {
    if let Some((status, body)) = http_get(port, "/", None) {
        return status == 401 || (status == 200 && looks_like_dsh(&body));
    }
    false
}

/// True when something is already bound to `port` on loopback.
fn port_in_use(port: u16) -> bool {
    format!("127.0.0.1:{port}")
        .parse()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok())
        .is_some()
}

fn next_free_port(start: u16) -> Option<u16> {
    (start..start + 256).find(|p| !port_in_use(*p))
}

// ---------------------------------------------------------------------------
// Engine process management
// ---------------------------------------------------------------------------

fn spawn_engine(app: &AppHandle, port: u16, cwd: &PathBuf) -> Result<(Child, String), String> {
    if !cwd.is_dir() {
        return Err(format!("workspace folder does not exist: {}", cwd.display()));
    }

    let dsh = find_dsh().ok_or_else(|| {
        "DeepSeek Harness (`dsh`) is not installed on this machine.\n\n\
         Install it once, then reopen this app:\n\
         \n  npm i -g @deepseek-ai/dsh\n\
         \n(requires Node.js 18+; this finds both global installs and the npm/npx cache)"
            .to_string()
    })?;

    let mut cmd = Command::new(&dsh);
    cmd.args(["web", "--no-open", "--port", &port.to_string()]);
    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Ok(alt) = std::env::var("DSH_DESKTOP_HOME") {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let resolved = if alt.starts_with('/') { PathBuf::from(&alt) } else { home.join(&alt) };
        cmd.env("DSH_HOME", resolved);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch the dsh engine: {e}"))?;

    // Capture stdout to parse the token URL that new dsh versions print.
    // Also forward lines to our own stdout for debugging.
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(stream) = stdout {
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                println!("[dsh:out] {line}");
                if let Some(url) = parse_dsh_url(&line) {
                    let _ = tx.send(url);
                }
            }
        });
    }
    drain_pipes(child.stderr.take(), "dsh:err");

    emit_status(app, "starting", Some(format!("booting engine on port {port}…")), None);
    logf(&format!("spawn_engine: child_pid={}", child.id()));
    let start = Instant::now();
    while start.elapsed() < BOOT_TIMEOUT {
        // Drain any token URL the stdout capture thread found.
        let mut tok = None;
        while let Ok(t) = rx.try_recv() {
            tok = Some(t);
        }
        if let Some(ref t) = tok {
            logf(&format!("spawn_engine: token_url captured ({})", t.len()));
        }

        if server_up(port) {
            let token_url = tok.unwrap_or_default();
            return Ok((child, token_url));
        }
        if let Ok(Some(status)) = child.try_wait() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("engine exited early with {status}"));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "timed out after {}s waiting for the engine on port {port}. \
         Check that `dsh web` works from a terminal.",
        BOOT_TIMEOUT.as_secs()
    ))
}

/// Parse a `dsh web: http://127.0.0.1:<port>/?token=<token>` line.
/// Returns the full URL string including the token query parameter.
fn parse_dsh_url(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = line.strip_prefix("dsh web:").or_else(|| line.strip_prefix("dsh:"))?;
    let url = rest.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

/// Forward engine stdout/stderr lines to our own stdout (visible when running
/// `tauri dev` / from the packaged app's log) instead of letting the pipes
/// fill up (a full pipe would block the engine).
fn drain_pipes<R: Read + Send + 'static>(stream: Option<R>, tag: &'static str) {
    if let Some(stream) = stream {
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                println!("[{tag}] {line}");
            }
        });
    }
}

fn stop_child(child: &mut Child) {
    let pid = child.id() as i32;
    // Graceful SIGTERM to the whole process group, then SIGKILL as fallback.
    #[cfg(unix)]
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = child.wait();
}

/// Watches a spawned engine for unexpected exit and notifies the user.
fn spawn_watchdog(app: &AppHandle, port: u16) {
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3)); // let the boot settle
        loop {
            let _state = {
                let state = handle.state::<AppState>();
                let mut engine = state.engine.lock().unwrap();
                if engine.port != Some(port) {
                    break; // slot reassigned or engine stopped
                }
                if let Some(child) = engine.child.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        if !engine.manual_stop {
                            let msg = format!(
                                "The engine exited unexpectedly ({status}). Restart it from the tray or menu."
                            );
                            emit_status(&handle, "error", Some(msg.clone()), None);
                            notify(&handle, "DSH engine stopped", &msg);
                        }
                        engine.child = None;
                        engine.port = None;
                        engine.url = None;
                        break;
                    }
                }
            };
            std::thread::sleep(Duration::from_millis(700));
        }
    });
}

// ---------------------------------------------------------------------------
// Actions (shared by tray, menu and IPC commands)
// ---------------------------------------------------------------------------

fn action_stop_engine(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut engine = state.engine.lock().unwrap();
    if let Some(mut child) = engine.child.take() {
        engine.manual_stop = true;
        stop_child(&mut child);
    }
    engine.mode = "stopped".to_string();
    engine.url = None;
    engine.token_url = None;
    engine.port = None;
    drop(engine);
    logf("engine stopped (manual stop)");
    emit_status(app, "stopped", Some("engine stopped".to_string()), None);
}

fn action_attach(app: &AppHandle, port: u16) -> Result<EngineInfo, String> {
    // With new auth-gated DSH, we cannot navigate to an existing engine without
    // the token. Probe whether it's accessible (200) or requires auth (401).
    if let Some((status, _)) = http_get(port, "/", None) {
        if status == 401 {
            return Err(format!(
                "a DSH engine is running on port {port} but requires authentication. \
                 Close it and reopen DSH Desktop to start a fresh engine with a token."
            ));
        }
    }
    let state = app.state::<AppState>();
    {
        let mut engine = state.engine.lock().unwrap();
        engine.mode = "attached".to_string();
        engine.port = Some(port);
        engine.url = Some(engine_url(port));
        engine.token_url = None;
        engine.child = None;
    }
    let url = engine_url(port);
    logf(&format!("attached to engine at {url}"));
    emit_status(
        app,
        "attached",
        Some("attached to the running engine".to_string()),
        Some(url.clone()),
    );
    navigate_main(app, &url);
    Ok(snapshot_info(&state))
}

fn action_start(app: &AppHandle) -> Result<EngineInfo, String> {
    let state = app.state::<AppState>();
    // Serialize boot: a second start_engine call blocks here, then hits the
    // "already healthy" branch below instead of spawning a duplicate engine.
    let _boot_guard = state.boot_lock.lock().unwrap();

    // 1) An engine we manage is already healthy?
    {
        let healthy = {
            let mut engine = state.engine.lock().unwrap();
            match engine.child.as_mut() {
                Some(child) => {
                    let alive = child.try_wait().ok().flatten().is_none();
                    engine.mode == "standalone"
                        && engine.port.is_some()
                        && alive
                        && engine.port.is_some_and(server_up)
                }
                None => false,
            }
        };
        if healthy {
            let port = engine_port(&state);
            let _ = healthy;
            let settings = state.settings.lock().unwrap();
            let engine = state.engine.lock().unwrap();
            return Ok(EngineInfo {
                status: "ready".to_string(),
                mode: "standalone".to_string(),
                url: engine.token_url.clone().or(Some(engine_url(port))),
                port: Some(port),
                pid: None,
                workspace: settings.workspace_dir().display().to_string(),
                detail: None,
                token_url: engine.token_url.clone(),
            });
        }
    }

    // 2) A DSH server already owns the canonical port → attach to it.
    let no_attach = std::env::var("DSH_DESKTOP_NO_ATTACH").is_ok();
    if !no_attach && is_dsh(CANONICAL_PORT) {
        match action_attach(app, CANONICAL_PORT) {
            Ok(info) => return Ok(info),
            Err(e) => logf(&format!("attach failed ({e}), falling back to standalone")),
        }
    }

    // 3) Spawn our own engine on the preferred (or next free) port.
    let settings = state.settings.lock().unwrap();
    let prefer = std::env::var("DSH_DESKTOP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or(settings.prefer_port)
        .unwrap_or(DESKTOP_PORT);
    let cwd = settings.workspace_dir();
    drop(settings);

    let port = if !port_in_use(prefer) {
        prefer
    } else {
        next_free_port(prefer + 1).ok_or_else(|| "no free port available".to_string())?
    };

    println!("[dsh-desktop] spawning engine on port {port}, cwd: {}", cwd.display());
    let (child, token_url) = spawn_engine(app, port, &cwd)?;
    logf(&format!("engine ready, pid {}", child.id()));
    let pid = child.id();
    let url = engine_url(port);
    {
        let mut engine = state.engine.lock().unwrap();
        engine.child = Some(child);
        engine.mode = "standalone".to_string();
        engine.port = Some(port);
        engine.url = Some(url.clone());
        engine.token_url = Some(token_url.clone());
        engine.manual_stop = false;
    }
    spawn_watchdog(app, port);

    // Auth-token flow (DSH ≥ 0.1.2).
    //
    // Facts established by diagnosis:
    //   * Sync tauri commands run on the MAIN thread; sleeping here would block
    //     the event loop, so with_webview/eval/navigate messages would only be
    //     processed after start_engine returns — hence the whole dance runs on
    //     a background thread.
    //   * WKWebView drops Set-Cookie from 3xx redirect responses, so DSH's
    //     token→cookie 303 never yields an authenticated `/` on its own. We
    //     fetch the cookie value ourselves and write it via document.cookie
    //     (synchronous browser machinery) once the engine origin is committed,
    //     then reload same-origin.
    //
    // Steps (all on a background thread):
    //   1) loadRequest the token URL → 303 → commits the engine's 401 shell
    //      (origin 127.0.0.1) as the document.
    //   2) Wait for that commit.
    //   3) document.cookie = '<name>=<value>; path=/' on that origin.
    //   4) Same-origin reload → cookie attached → real DSH loads.
    if !token_url.is_empty() {
        let app2 = app.clone();
        let token2 = token_url.clone();
        std::thread::spawn(move || {
            webview_load_url(&app2, &token2);
            std::thread::sleep(Duration::from_millis(3000));

            if let Some(nv) = fetch_auth_cookie_name_value(port, &token2) {
                let esc = nv.replace('\\', "\\\\").replace('\'', "\\'");
                eval_fire(
                    &app2,
                    "cookie-write",
                    &format!("document.cookie = '{esc}; path=/'; 'ok'"),
                );
                std::thread::sleep(Duration::from_millis(700));
                eval_fire(&app2, "reload", "location.reload(); 'ok'");
            } else {
                logf("WEBV no cookie to write (auth exchange failed)");
            }
        });
    } else {
        emit_status(app, "ready", Some("engine ready".to_string()), Some(url.clone()));
        navigate_main(app, &url);
    }

    // Diagnostic: a few seconds later, log what the webview rendered so we can
    // verify the auth cookie actually let DSH load (title/body text).
    {
        let app2 = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(12));
            diag_page_content(&app2);
        });
    }
    let mut info = snapshot_info(&state);
    info.pid = Some(pid);
    Ok(info)
}

fn restart_engine(app: &AppHandle) {
    action_stop_engine(app);
    // The controller page re-runs start_engine when it remounts.
    navigate_to_controller(app);
}

// ---------------------------------------------------------------------------
// Settings persistence
// ---------------------------------------------------------------------------

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    match settings_path(app) {
        Some(path) => fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
            .unwrap_or_default(),
        None => Settings::default(),
    }
}

fn persist_settings(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    if let Some(path) = settings_path(app) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&settings) {
            let _ = fs::write(path, json);
        }
    }
}

// ---------------------------------------------------------------------------
// Tray + menu
// ---------------------------------------------------------------------------

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("show", "Show DSH Desktop").build(app)?;
    let open_browser = MenuItemBuilder::with_id("open-browser", "Open in Browser…").build(app)?;
    let set_workspace = MenuItemBuilder::with_id("set-workspace", "Set Workspace Folder…").build(app)?;
    let restart = MenuItemBuilder::with_id("restart", "Restart Engine").build(app)?;
    let stop = MenuItemBuilder::with_id("stop", "Stop Engine").build(app)?;
    let open_dsh_home = MenuItemBuilder::with_id("open-appdata", "Reveal Data Folder (~/.dsh)").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit DSH Desktop").build(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &open_browser,
            &set_workspace,
            &PredefinedMenuItem::separator(app)?,
            &restart,
            &stop,
            &open_dsh_home,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .expect("the default window icon is missing (tauri-build should embed icons/)");

    TrayIconBuilder::with_id("dsh-tray")
        .icon(icon)
        .tooltip("DSH Desktop — DeepSeek Harness")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| handle_menu_action(app, event.id().as_ref()))
        .build(app)?;
    Ok(())
}

fn build_app_menu(app: &AppHandle) -> tauri::Result<()> {
    let about = PredefinedMenuItem::about(app, None, None)?;
    let services = PredefinedMenuItem::services(app, None)?;
    let hide = PredefinedMenuItem::hide(app, None)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit DSH Desktop").build(app)?;

    let show = MenuItemBuilder::with_id("show", "Show DSH Desktop").build(app)?;
    let open_browser = MenuItemBuilder::with_id("open-browser", "Open in Browser…").build(app)?;
    let set_workspace = MenuItemBuilder::with_id("set-workspace", "Set Workspace Folder…").build(app)?;
    let restart = MenuItemBuilder::with_id("restart", "Restart Engine").build(app)?;
    let stop = MenuItemBuilder::with_id("stop", "Stop Engine").build(app)?;
    let open_dsh_home = MenuItemBuilder::with_id("open-appdata", "Reveal Data Folder (~/.dsh)").build(app)?;

    let app_menu = SubmenuBuilder::new(app, "DSH Desktop")
        .items(&[&about, &services, &hide, &quit])
        .build()?;

    // Standard Edit menu: without it, AppKit has no Cmd+C/X/V/A key
    // equivalents and the WKWebView text fields won't accept copy/paste
    // shortcuts (typing still works, but command shortcuts are swallowed).
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .build()?;

    let engine_menu = SubmenuBuilder::new(app, "Engine")
        .items(&[&show, &open_browser, &set_workspace, &restart, &stop, &open_dsh_home])
        .build()?;

    let menu = Menu::with_items(app, &[&app_menu, &edit_menu, &engine_menu])?;
    app.set_menu(menu)?;
    Ok(())
}

fn handle_menu_action(app: &AppHandle, id: &str) {
    match id {
        "show" => show_window(app),
        "open-browser" => {
            let port = app.state::<AppState>().engine.lock().unwrap().port;
            match port {
                Some(port) => {
                    let handle = app.clone();
                    std::thread::spawn(move || {
                        let _ =
                            handle.opener().open_url(format!("http://127.0.0.1:{port}"), None::<&str>);
                    });
                }
                None => show_window(app),
            }
        }
        "set-workspace" => pick_workspace_and_restart(app),
        "restart" => restart_engine(app),
        "stop" => action_stop_engine(app),
        "open-appdata" => {
            let handle = app.clone();
            std::thread::spawn(move || {
                if let Some(dsh_home) = home_dir().map(|h| h.join(".dsh")) {
                    let _ = handle.opener().open_path(dsh_home.display().to_string(), None::<&str>);
                }
            });
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Native folder picker → save the workspace → restart the engine in it.
fn pick_workspace_and_restart(app: &AppHandle) {
    use tauri_plugin_dialog::DialogExt;
    let handle = app.clone();
    let picked = handle
        .dialog()
        .file()
        .set_title("Choose the workspace folder (DSH working directory)")
        .pick_folder(move |folder| {
            if let Some(path) = folder {
                if let Ok(path) = path.into_path() {
                    {
                        let state = handle.state::<AppState>();
                        let mut settings = state.settings.lock().unwrap();
                        settings.workspace = Some(path.clone());
                    }
                    persist_settings(&handle);
                    restart_engine(&handle);
                }
            }
        });
    let _ = picked;
}

// ---------------------------------------------------------------------------
// IPC commands (invoked only by the bundled controller page)
// ---------------------------------------------------------------------------

#[tauri::command]
fn start_engine(app: AppHandle) -> Result<EngineInfo, String> {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    logf(&format!("start_engine #[{seq}] invoked"));
    let result = action_start(&app);
    match &result {
        Ok(i) => logf(&format!("start_engine #[{seq}] -> ok status={} url={:?}", i.status, i.url)),
        Err(e) => logf(&format!("start_engine #[{seq}] -> err: {e}")),
    }
    result
}

#[tauri::command]
fn stop_engine(app: AppHandle) -> EngineInfo {
    action_stop_engine(&app);
    snapshot_info(&app.state::<AppState>())
}

#[tauri::command]
fn engine_info(app: AppHandle) -> EngineInfo {
    snapshot_info(&app.state::<AppState>())
}

#[tauri::command]
fn set_workspace(app: AppHandle) {
    pick_workspace_and_restart(&app);
}

#[tauri::command]
fn open_in_browser(app: AppHandle) -> Result<(), String> {
    let port = app.state::<AppState>().engine.lock().unwrap().port;
    match port {
        Some(port) => app
            .opener()
            .open_url(format!("http://127.0.0.1:{port}"), None::<&str>)
            .map_err(|e| e.to_string()),
        None => Err("no engine is running yet".to_string()),
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Paste file paths (macOS convenience)
// ---------------------------------------------------------------------------
// In Finder, Cmd+C on a file/folder puts a *file reference* on the pasteboard
// (public.file-url), not text. Pasting that inside a web text field yields
// nothing. We install a local NSEvent monitor for Cmd+V: if the pasteboard is
// a "pure file copy" (files but no meaningful text), we rewrite the pasteboard
// to the POSIX path(s) right before the normal paste action runs, so the DSH
// input box receives the path as ordinary text. Plain-text copies are untouched.

const KEY_V: u16 = 9; // kVK_ANSI_V

/// extern statics are unsafe to read; these bind the pasteboard type constants
/// used below into plain references.
fn paste_type_file_url() -> &'static NSString {
    unsafe { &*NSPasteboardTypeFileURL }
}
fn paste_type_string() -> &'static NSString {
    unsafe { &*NSPasteboardTypeString }
}

/// Read every file URL off the general pasteboard, returning POSIX paths.
fn pasteboard_paths() -> Vec<String> {
    let pb = NSPasteboard::generalPasteboard();
    let items = match pb.pasteboardItems() {
        Some(items) => items,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in items.iter() {
        if let Some(url) = item.stringForType(paste_type_file_url()) {
            if let Some(path) = file_url_to_path(&url.to_string()) {
                out.push(path);
            }
        }
    }
    out
}

/// True when the pasteboard holds a Finder-style file copy: at least one file
/// URL, and any plain text is just the file name(s). Finder copies always
/// include the basename as text, so we match that instead of demanding "no
/// text at all".
fn is_file_copy() -> bool {
    let paths = pasteboard_paths();
    if paths.is_empty() {
        return false;
    }
    let pb = NSPasteboard::generalPasteboard();
    if let Some(text) = pb.stringForType(paste_type_string()) {
        let t = text.to_string();
        let lines: Vec<&str> = t.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if !lines.is_empty() {
            let bases: Vec<&str> = paths
                .iter()
                .map(|p| p.rsplit('/').next().unwrap_or(""))
                .collect();
            let all_are_names = lines.iter().all(|l| bases.contains(l));
            if !all_are_names {
                return false; // real text content present → normal paste
            }
        }
    }
    true
}

/// Resolve a pasteboard file URL to its real POSIX path via NSURL.
/// Handles plain "file:///…" URLs AND the opaque "file:///.file/id=…"
/// file-reference URLs that some copies (browser, apps) store — NSURL.path
/// resolves both, with percent-decoding included.
fn file_url_to_path(url: &str) -> Option<String> {
    let ns = NSURL::URLWithString(&NSString::from_str(url.trim()))?;
    ns.path().map(|p| p.to_string())
}

/// Install an app-lifetime local monitor: on Cmd+V with a file-only copy,
/// replace the pasteboard content with the paths so the webview pastes text.
fn install_paste_path_monitor() {
    let block: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent> =
        RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            let event_ref = unsafe { event.as_ref() };
            let is_cmd_v = event_ref.keyCode() == KEY_V
                && event_ref.modifierFlags().contains(NSEventModifierFlags::Command);
            if is_cmd_v {
                if is_file_copy() {
                    let paths = pasteboard_paths();
                    if !paths.is_empty() {
                        let text = if paths.len() == 1 {
                            paths[0].clone()
                        } else {
                            paths.join("\n")
                        };
                        let pb = NSPasteboard::generalPasteboard();
                        pb.clearContents();
                        let _ =
                            pb.setString_forType(&NSString::from_str(&text), paste_type_string());
                        logf(&format!(
                            "paste-path: injected {} path(s): {}",
                            paths.len(),
                            text
                        ));
                    }
                } else {
                    logf("paste-path: Cmd+V seen but pasteboard is NOT a file copy");
                }
            }
            event.as_ptr() // return the event: paste proceeds as usual
        });

    // The returned opaque token keeps the monitor installed for the lifetime
    // of the process; we intentionally leak it.
    let token = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    std::mem::forget(token);
    logf("paste-path: Cmd+V monitor installed (Finder file copies paste as paths)");
}

// ---------------------------------------------------------------------------
// App entry
// ---------------------------------------------------------------------------

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second launch only focuses the existing window.
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            start_engine,
            stop_engine,
            engine_info,
            open_in_browser,
            set_workspace
        ])
        .setup(|app| {
            // Capture our own controller URL before we ever navigate away.
            let initial = app
                .get_webview_window("main")
                .and_then(|w| w.url().ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| "index.html".to_string());

            let handle = app.handle();
            let settings = load_settings(handle);
            let state = AppState {
                engine: Mutex::new(Engine::default()),
                settings: Mutex::new(settings),
                boot_lock: Mutex::new(()),
                initial_url: Mutex::new(initial),
            };
            app.manage(state);

            build_app_menu(handle)?;
            build_tray(handle)?;

            // Finder-copy → path-text bridging for the DSH input box.
            install_paste_path_monitor();

            // Ask macOS for notification permission up front (clean first run).
            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().request_permission();

            // The engine is booted by the controller page: it calls
            // `start_engine` on load (covers the first launch AND the
            // "Restart Engine" tray flow, which navigates back to this page).
            // Keeping boot driver-side avoids a double-spawn race.
            Ok(())
        })
        .on_window_event(|window, event| {
            // macOS convention: the red button hides to the tray; quitting
            // happens via the menu/tray Quit item or Cmd+Q.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .on_menu_event(|app, event| handle_menu_action(app, event.id().as_ref()))
        .build(tauri::generate_context!())
        .expect("error while building the DSH Desktop application");

    app.run(|app_handle, event| match event {
        RunEvent::Exit => {
            // Tear the engine down so no orphan server keeps the port open.
            let state = app_handle.state::<AppState>();
            let mut engine = state.engine.lock().unwrap();
            if let Some(mut child) = engine.child.take() {
                engine.manual_stop = true;
                stop_child(&mut child);
            }
        }
        _ => {}
    });
}
