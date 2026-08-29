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

/// Timestamped diagnostic logger (seconds since app start).
fn logf(msg: &str) {
    println!("[{:.1}s] [dsh-desktop] {msg}", whole().elapsed().as_secs_f64());
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

/// Synchronous GET `path` on 127.0.0.1:`port`. Returns (status, body) on
/// success, `None` when nothing answers or the read fails.
fn http_get(port: u16, path: &str) -> Option<(u16, Vec<u8>)> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse().ok()?, PROBE_TIMEOUT).ok()?;
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
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
    matches!(http_get(port, "/"), Some((200, _)))
}

fn looks_like_dsh(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(body);
    head.contains("__DSH_BOOT__") || head.contains("DeepSeek Harness")
}

fn is_dsh(port: u16) -> bool {
    matches!(http_get(port, "/"), Some((200, ref b)) if looks_like_dsh(b))
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

fn spawn_engine(app: &AppHandle, port: u16, cwd: &PathBuf) -> Result<Child, String> {
    if !cwd.is_dir() {
        return Err(format!("workspace folder does not exist: {}", cwd.display()));
    }

    // The app requires `dsh` to be installed (like an ordinary `dsh web` run).
    // We deliberately do NOT download/install it from inside the app — users
    // install DeepSeek Harness on their own, and this app just finds it.
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
    // Own process group so we can signal the whole tree (DSH subprocesses)
    // with a single SIGTERM/SIGKILL.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Dev/test hook: allow an isolated DSH_HOME so a standalone run never
    // touches a live `dsh web` store. Inherits the parent env otherwise.
    if let Ok(alt) = std::env::var("DSH_DESKTOP_HOME") {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let resolved = if alt.starts_with('/') { PathBuf::from(&alt) } else { home.join(&alt) };
        cmd.env("DSH_HOME", resolved);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch the dsh engine: {e}"))?;

    drain_pipes(child.stdout.take(), "dsh:out");
    drain_pipes(child.stderr.take(), "dsh:err");

    // Poll until the server answers; fail fast if the child exits early.
    emit_status(app, "starting", Some(format!("booting engine on port {port}…")), None);
    logf(&format!("spawn_engine: child_pid={}", child.id()));
    let start = Instant::now();
    while start.elapsed() < BOOT_TIMEOUT {
        if server_up(port) {
            return Ok(child);
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
    engine.port = None;
    drop(engine);
    logf("engine stopped (manual stop)");
    emit_status(app, "stopped", Some("engine stopped".to_string()), None);
}

fn action_attach(app: &AppHandle, port: u16) -> EngineInfo {
    let state = app.state::<AppState>();
    {
        let mut engine = state.engine.lock().unwrap();
        engine.mode = "attached".to_string();
        engine.port = Some(port);
        engine.url = Some(engine_url(port));
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
    snapshot_info(&state)
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
            return Ok(EngineInfo {
                status: "ready".to_string(),
                mode: "standalone".to_string(),
                url: Some(engine_url(port)),
                port: Some(port),
                pid: None,
                workspace: settings.workspace_dir().display().to_string(),
                detail: None,
            });
        }
    }

    // 2) A DSH server already owns the canonical port → attach to it.
    let no_attach = std::env::var("DSH_DESKTOP_NO_ATTACH").is_ok();
    if !no_attach && is_dsh(CANONICAL_PORT) {
        return Ok(action_attach(app, CANONICAL_PORT));
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
    let child = spawn_engine(app, port, &cwd)?;
    logf(&format!("engine ready, pid {}", child.id()));
    let pid = child.id();
    let url = engine_url(port);
    {
        let mut engine = state.engine.lock().unwrap();
        engine.child = Some(child);
        engine.mode = "standalone".to_string();
        engine.port = Some(port);
        engine.url = Some(url.clone());
        engine.manual_stop = false;
    }
    spawn_watchdog(app, port);

    emit_status(app, "ready", Some("engine ready".to_string()), Some(url.clone()));
    navigate_main(app, &url);
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
    let engine_menu = SubmenuBuilder::new(app, "Engine")
        .items(&[&show, &open_browser, &set_workspace, &restart, &stop, &open_dsh_home])
        .build()?;

    let menu = Menu::with_items(app, &[&app_menu, &engine_menu])?;
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
