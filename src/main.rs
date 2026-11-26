// A console subsystem binary gets a black console window behind it on Windows. A
// release build is a GUI program, so double clicking gives one window and nothing
// else. A debug build keeps the console, because that is where its messages go.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use cef::*;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod update;

const APP_ID: &str = "1574411473172561931";
const APP_NAME: &str = "Pokem Online";
const GAME_URL: &str = "https://pokem.onl/";

/// Chromium's own agent for the platform, with two tokens added. `pokem-online` says
/// who we are, and `Electron/` is the token the site looks for to tell its desktop app
/// from a browser tab: without it, the character screen carries an offer to download
/// the official launcher (`bCe()` in the game bundle returns a platform for a plain
/// browser agent and null for anything matching `/Electron\//i`).
#[cfg(target_os = "linux")]
const UA_PLATFORM: &str = "X11; Linux x86_64";
#[cfg(target_os = "macos")]
const UA_PLATFORM: &str = "Macintosh; Intel Mac OS X 10_15_7";
#[cfg(target_os = "windows")]
const UA_PLATFORM: &str = "Windows NT 10.0; Win64; x64";

fn user_agent() -> String {
    // The version in the UA follows the build, so it cannot drift from the release.
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "Mozilla/5.0 ({UA_PLATFORM}) AppleWebKit/537.36 (KHTML, like Gecko) \
         pokem-online/{version} Chrome/156.0.0.0 Electron/156.0.0 Safari/537.36"
    )
}

/// The renderer dying twice inside a minute means the page is dying as it loads, so the
/// recovery goes by way of the offline page instead of spinning on a broken page.
const CRASH_GAP_MS: u64 = 60_000;

/// Accelerator ids for the keys a bare Chromium window does not wire up itself.
const CMD_FULLSCREEN: i32 = 1;
const CMD_RELOAD: i32 = 2;
const CMD_WIKI: i32 = 3;
const CMD_MUTE: i32 = 4;
/// F11, F5, F1 and End from Chromium's keyboard codes.
const KEY_F11: i32 = 0x7a;
const KEY_F5: i32 = 0x74;
const KEY_F1: i32 = 0x70;
const KEY_END: i32 = 0x23;
/// The wiki is a separate page on the game's own origin, and it reads its own data.
const WIKI_URL: &str = "https://pokem.onl/wiki";

/// Where the newest release announces itself, and where the notice sends the player.
const RELEASE_API: &str =
    "https://api.github.com/repos/ArisSwift/Pokemon-Online-Launcher/releases/latest";
const RELEASE_PAGE: &str =
    "https://github.com/ArisSwift/Pokemon-Online-Launcher/releases/latest";
/// A day is often enough for a release notice, and it keeps the API call rare.
const UPDATE_INTERVAL_SECS: u64 = 24 * 60 * 60;

static LAST_RECOVERY_MS: AtomicU64 = AtomicU64::new(0);
/// Set while the browser is closing, so a renderer going away is not mistaken for a
/// crash and reloaded on the way out.
static CLOSING: AtomicBool = AtomicBool::new(false);
/// Devtools opens once, not on every navigation.
static DEVTOOLS_OPENED: AtomicBool = AtomicBool::new(false);

/// The game's window, kept so a second launch can bring it forward. CEF owns the object,
/// so it is only touched from a task on the UI thread; this is just a place to hold a
/// reference until that task runs.
static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

/// The map's own name, as the minimap spells it. That is the only thing the wiki key
/// needs: the wiki resolves a place name to a map page, and a floor name resolves to
/// nothing, because buildings are not places in its map data.
static WHERE: Mutex<Option<String>> = Mutex::new(None);

/// The page's browser, held so the update task can reach its main frame long after the
/// network round trip that decided the notice.
static BROWSER: Mutex<Option<Browser>> = Mutex::new(None);
/// The wiki's window and browser, kept apart from the game's. It carries a client of its
/// own, so closing it closes the wiki and nothing else.
static WIKI_WINDOW: Mutex<Option<Window>> = Mutex::new(None);
static WIKI_BROWSER: Mutex<Option<Browser>> = Mutex::new(None);
/// End. Session only: a muted game is not a preference worth remembering.
static MUTED: AtomicBool = AtomicBool::new(false);

/// Silence or unsilence both of this process's pages. The wiki has no sound of its
/// own today, but muting it with the game keeps the shortcut honest whichever window
/// has focus.
fn toggle_mute() {
    let muted = !MUTED.load(Ordering::Relaxed);
    MUTED.store(muted, Ordering::Relaxed);
    for slot in [&BROWSER, &WIKI_BROWSER] {
        if let Ok(browser) = slot.lock() {
            if let Some(browser) = browser.as_ref() {
                if let Some(host) = browser.host() {
                    host.set_audio_muted(muted as i32);
                }
            }
        }
    }
}
/// The newest release, kept so a window built after the answer still shows it.
static UPDATE_READY: Mutex<Option<String>> = Mutex::new(None);
/// A notice is a heads up, not a nag: it is injected once per run.
static NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Where the in place update has got to. The pill reads this, so it is a small state
/// machine rather than a log.
#[derive(Clone, Debug)]
enum UpdatePhase {
    Idle,
    Downloading(u8),
    Ready,
    Failed(String),
    Unsupported,
}

static UPDATE_PHASE: Mutex<UpdatePhase> = Mutex::new(UpdatePhase::Idle);
/// The checked copy waiting to be renamed into place.
static UPDATE_STAGED: Mutex<Option<update::Target>> = Mutex::new(None);
/// One download at a time, however many times the pill is clicked.
static UPDATE_BUSY: AtomicBool = AtomicBool::new(false);

fn set_phase(phase: UpdatePhase) {
    if let Ok(mut held) = UPDATE_PHASE.lock() {
        *held = phase;
    }
    let mut task = PaintUpdate::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct RaiseWindow {}

    impl Task {
        fn execute(&self) {
            let window = WINDOW.lock().ok().and_then(|held| held.clone());
            let Some(window) = window else { return };
            if window.is_closed() != 0 {
                return;
            }
            if window.is_minimized() != 0 {
                window.restore();
            }
            if debug_echo() {
                eprintln!("bringing the window forward");
            }
            window.bring_to_top();
            window.activate();
        }
    }
}

wrap_task! {
    struct ShowUpdateNotice {
        version: String,
    }

    impl Task {
        fn execute(&self) {
            apply_update_notice(&self.version);
        }
    }
}

wrap_task! {
    struct PaintUpdate {}

    impl Task {
        fn execute(&self) {
            paint_update();
        }
    }
}

// POKEM_WIKI opens the wiki on launch. Pressing F1 needs a desktop and a hand; this
// lets the window be checked from a terminal, alongside POKEM_DEBUG_PORT.
wrap_task! {
    struct ShowWiki {
        url: String,
    }

    impl Task {
        fn execute(&self) {
            if debug_echo() {
                eprintln!("wiki task: opening {}", self.url);
            }
            open_wiki(&self.url);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The launcher icon, decoded by CEF itself. `add_png` takes the file as-is, so the
/// repo keeps one PNG and there is no image crate in the build.
fn load_icon() -> Option<Image> {
    let image = image_create()?;
    image.add_png(1.0, Some(include_bytes!("../Icon/icon.png")));
    Some(image)
}

/// What the page last told us about itself. Nothing here is reachable from outside the
/// page, so a small script inside it reports these fields and the console handler picks
/// them up.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    name: Option<String>,
    place: Option<String>,
    /// `world:kanto`, taken straight from the game's own presence socket.
    region: Option<String>,
    /// `solo`, `online` or `tower`, the game's own word for what you are doing.
    activity: Option<String>,
}

static PRESENCE_TX: OnceLock<mpsc::Sender<Snapshot>> = OnceLock::new();

fn debug_echo() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("POKEM_DEBUG").is_some())
}

/// Injected into the game page once per load. It logs a single `PO {...}` line every
/// few seconds; `on_console_message` turns those into presence updates.
///
/// Two things are worth knowing about why it looks like this:
/// - The game publishes its own presence to `/api/presence` as
///   `{t:"state", state:"solo"|"online"|"tower", region:"world:kanto"}`. Wrapping
///   `WebSocket.send` before the game chunk loads means we mirror the game instead of
///   re-deriving the region from map data we cannot see.
/// - The trainer name is in `localStorage["pmp.session@https://pokem.onl"]`.
const PROBE_JS: &str = r##"
(function () {
  if (window.__pokemProbe) return;
  window.__pokemProbe = true;

  // Patch the prototype, not an instance: the game creates its sockets well after this
  // runs and keeps them privately.
  const nativeSend = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    try {
      if (typeof data === "string" && data.charCodeAt(0) === 123 && data.indexOf('"state"') > 0) {
        const msg = JSON.parse(data);
        if (msg && msg.t === "state") {
          window.__poWire = { state: msg.state || null, region: msg.region || null };
        }
      }
    } catch (_) {}
    return nativeSend.call(this, data);
  };

  function deepQueryAll(selector) {
    const found = [];
    const walk = (root) => {
      for (const el of root.querySelectorAll(selector)) found.push(el);
      for (const el of root.querySelectorAll("*")) {
        if (el.shadowRoot) walk(el.shadowRoot);
      }
    };
    walk(document);
    return found;
  }

  function textOf(node) {
    if (!node) return null;
    const text = node.textContent.replace(/\s+/g, " ").trim();
    return text || null;
  }

  // The wiki's places table matches a row on a plain substring, so "Goldenrod Poké
  // Center 1F" finds nothing while "Goldenrod" finds the map. The minimap stacks the
  // building and the floor under the map name, and innerText keeps those breaks where
  // textContent glues them into one run, so keep only the part that names a map.
  function mapName(node) {
    if (!node) return null;
    const raw = node.innerText || node.textContent || "";
    const first = raw.split(/\r?\n/)
      .map(function (line) { return line.replace(/\s+/g, " ").trim(); })
      .find(function (line) { return line.length > 0; });
    if (!first) return null;
    const head = first.split(/\s+[|\u00b7\u2022\u2014\u2013]\s+/)[0].trim();
    const trimmed = head
      .replace(/\s+\d+(?:st|nd|rd|th)\s+floor$/i, "")
      .replace(/\s+(?:B?\d{1,2}F|F\d{1,2})$/i, "")
      .trim();
    return trimmed || null;
  }

  function mapLines(node) {
    if (!node) return [];
    const raw = node.innerText || node.textContent || "";
    return raw.split(/\r?\n/)
      .map(function (line) { return line.replace(/\s+/g, " ").trim(); })
      .filter(function (line) { return line.length > 0; })
      .slice(0, 4);
  }

  // Where you are, richest first. `#where` names the floor when you are inside a
  // building; outdoors it is empty and `#minimap` holds the town or route name. Both are
  // plain elements in the game's own document.
  function placeText() {
    for (const el of deepQueryAll("#where")) {
      const text = textOf(el);
      if (text) return text;
    }
    return textOf(document.getElementById("minimap"));
  }

  function session() {
    try {
      let key = null;
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k && k.indexOf("pmp.session@") === 0) key = k;
      }
      return key ? JSON.parse(localStorage.getItem(key) || "null") : null;
    } catch (_) {
      return null;
    }
  }

  function report() {
    try {
      const s = session();
      const chars = s && Array.isArray(s.characters) ? s.characters : [];
      const index = s && typeof s.characterIndex === "number" ? s.characterIndex : 0;
      const character = chars[index] || chars[0] || null;
      const wire = window.__poWire || {};

      console.log("PO " + JSON.stringify({
        name: (character && (character.name || character.nickname)) ||
              (s && s.user && (s.user.name || s.user.username)) || null,
        place: placeText(),
        // The minimap names the map itself, indoors included, which is what the wiki
        // indexes. `place` above is richer but a floor name, so keep both.
        map: mapName(document.getElementById("minimap")),
        // Only read when POKEM_DEBUG is on, to see what the minimap really stacks.
        mapLines: mapLines(document.getElementById("minimap")),
        region: wire.region || null,
        activity: wire.state || null,
        session: !!s
      }));
    } catch (e) {
      console.log("PO ERR " + (e && e.message));
    }
  }

  report();
  setInterval(report, 5000);
})();
"##;

/// Where "Which files?" goes. Empty hides the link entirely. The page it points at
/// explains which dumps the game wants; it does not hand out the files.
const FILES_HELP_URL: &str = "https://alexstrip.rf.gd/game-files-v2.html";

/// The game says what it is missing but never says what those games are called, so a new
/// player meets a checklist of names they do not recognise and a picker that cannot tell
/// them what it wants. This adds one glass pill to the two places the game says files are
/// missing. The pill is an icon that opens its label on hover, and clicking it opens a
/// panel inside the window.
///
/// The panel is ours rather than a page in a browser: it works offline, it cannot 404,
/// and it says the short version. The full page is one button away inside it for anyone
/// who wants the copying tools.
const FILES_HELP_JS: &str = r##"
(function () {
  if (window.__pokemFilesHelp) return;
  window.__pokemFilesHelp = true;

  var HELP = "__FILES_HELP__";
  var PANEL = "pokem-files-panel";
  var STYLE = "pokem-files-style";

  var TRIGGER_CSS = [
    "a[data-pokem-help].pkh-trigger{all:initial;box-sizing:border-box;display:inline-flex;",
    "align-items:center;height:34px;padding:0 8px;border-radius:999px;cursor:pointer;",
    "background:rgba(255,255,255,.055);border:1px solid rgba(255,255,255,.14);",
    "backdrop-filter:blur(14px) saturate(150%);-webkit-backdrop-filter:blur(14px) saturate(150%);",
    "box-shadow:0 6px 18px rgba(0,0,0,.18),inset 0 1px 0 rgba(255,255,255,.08);",
    "color:#dfe6f4;text-decoration:none;vertical-align:middle;-webkit-tap-highlight-color:transparent;",
    "cursor:pointer;-webkit-user-select:none;user-select:none;",
    "transition:background .18s ease,border-color .18s ease,box-shadow .18s ease;}",
    "a[data-pokem-help].pkh-trigger:hover{background:rgba(255,255,255,.10);",
    "border-color:rgba(255,203,5,.42);",
    "box-shadow:0 8px 24px rgba(0,0,0,.26),inset 0 1px 0 rgba(255,255,255,.12);}",
    "a[data-pokem-help].pkh-trigger:focus-visible{outline:none;",
    "box-shadow:0 0 0 3px rgba(255,203,5,.28),0 8px 24px rgba(0,0,0,.26);}",
    "a[data-pokem-help].pkh-trigger .pkh-ico{all:initial;display:block;width:18px;height:18px;",
    "flex:none;color:#ffcb05;cursor:pointer;-webkit-user-select:none;user-select:none;",
    "transition:color .18s ease,transform .18s ease;}",
    "a[data-pokem-help].pkh-trigger:hover .pkh-ico,",
    "a[data-pokem-help].pkh-trigger:focus-visible .pkh-ico{color:#ffd83d;transform:scale(1.06);}",
    "a[data-pokem-help].pkh-trigger .pkh-label{all:initial;display:block;",
    "font:600 12.5px/1 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;",
    "color:#e6ecf9;white-space:nowrap;overflow:hidden;max-width:0;opacity:0;margin-left:0;",
    "cursor:pointer;-webkit-user-select:none;user-select:none;",
    "transition:max-width .3s cubic-bezier(.2,.8,.3,1),opacity .18s ease,",
    "margin-left .3s cubic-bezier(.2,.8,.3,1);}",
    "a[data-pokem-help].pkh-trigger:hover .pkh-label,",
    "a[data-pokem-help].pkh-trigger:focus-visible .pkh-label{max-width:min(240px,58vw);",
    "opacity:1;margin-left:8px;}",
    "@media (hover:none){a[data-pokem-help].pkh-trigger .pkh-label{",
    "max-width:min(240px,58vw);opacity:1;margin-left:8px;}}",
    "@media (prefers-reduced-motion:reduce){",
    "a[data-pokem-help].pkh-trigger,a[data-pokem-help].pkh-trigger *{transition:none;}}"
  ].join("");

  var PANEL_CSS = [
    "#" + PANEL + "{all:initial;position:fixed;inset:0;z-index:2147483647;display:flex;",
    "align-items:center;justify-content:center;padding:24px;",
    "background:rgba(4,7,14,.64);backdrop-filter:blur(9px);",
    "-webkit-backdrop-filter:blur(9px);color-scheme:dark;}",
    "#" + PANEL + " *:not(svg):not(svg *){all:initial;box-sizing:border-box;",
    "font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;}",
    "#" + PANEL + " .pkh-card{position:relative;display:flex;flex-direction:column;",
    "width:min(780px,100%);max-height:90vh;border-radius:20px;",
    "border:1px solid rgba(255,255,255,.16);",
    "background:linear-gradient(180deg,rgba(36,44,64,.90),rgba(12,16,26,.96));",
    "backdrop-filter:blur(24px) saturate(150%);",
    "-webkit-backdrop-filter:blur(24px) saturate(150%);",
    "box-shadow:0 34px 90px rgba(0,0,0,.62),inset 0 1px 0 rgba(255,255,255,.10);",
    "overflow:hidden;}",
    "#" + PANEL + " .pkh-card::before{content:'';position:absolute;left:0;right:0;top:0;",
    "height:3px;background:linear-gradient(90deg,#ee3b2b,#ffcb05 50%,#2a75bb);",
    "opacity:.9;}",
    "#" + PANEL + " .pkh-head{display:flex;align-items:flex-start;gap:14px;",
    "padding:20px 22px 16px;}",
    "#" + PANEL + " .pkh-ball{display:block;width:30px;height:30px;flex:none;margin-top:1px;",
    "filter:drop-shadow(0 4px 12px rgba(0,0,0,.45));}",
    "#" + PANEL + " .pkh-titles{display:flex;flex-direction:column;gap:4px;flex:1;min-width:0;}",
    "#" + PANEL + " .pkh-title{display:block;font:800 19px/1.2 system-ui,sans-serif;",
    "color:#f7f9fd;letter-spacing:-.01em;}",
    "#" + PANEL + " .pkh-sub{display:block;font:12.5px/1.45 system-ui,sans-serif;",
    "color:#98a4bc;}",
    "#" + PANEL + " .pkh-x{display:flex;align-items:center;justify-content:center;",
    "cursor:pointer;width:28px;height:28px;padding:0;flex:none;align-self:flex-start;",
    "border-radius:9px;border:1px solid rgba(255,255,255,.12);",
    "background:rgba(255,255,255,.05);color:#c9d2e2;",
    "font:600 15px/1 system-ui,sans-serif;}",
    "#" + PANEL + " .pkh-x:hover{background:rgba(255,255,255,.12);}",
    "#" + PANEL + " .pkh-body{display:grid;grid-template-columns:minmax(0,1.12fr) minmax(0,.88fr);",
    "padding:0 22px;overflow-y:auto;min-height:0;",
    "scrollbar-width:thin;scrollbar-color:rgba(255,255,255,.16) transparent;}",
    "#" + PANEL + " .pkh-body::-webkit-scrollbar{width:9px;}",
    "#" + PANEL + " .pkh-body::-webkit-scrollbar-thumb{background:rgba(255,255,255,.16);",
    "border-radius:9px;}",
    "#" + PANEL + " .pkh-body::-webkit-scrollbar-track{background:transparent;}",
    "#" + PANEL + " .pkh-col{padding:14px 0 16px;min-width:0;}",
    "#" + PANEL + " .pkh-col:first-child{padding-right:22px;}",
    "#" + PANEL + " .pkh-col + .pkh-col{border-left:1px solid rgba(255,255,255,.08);",
    "padding-left:24px;}",
    "#" + PANEL + " .pkh-h{display:flex;align-items:center;gap:9px;",
    "font:700 10.5px/1 system-ui,sans-serif;letter-spacing:.12em;text-transform:uppercase;",
    "color:rgba(255,203,5,.85);margin:16px 0 8px;}",
    "#" + PANEL + " .pkh-col > .pkh-h:first-child{margin-top:0;}",
    "#" + PANEL + " .pkh-h::after{content:'';height:1px;flex:1;",
    "background:linear-gradient(90deg,rgba(255,203,5,.22),transparent);}",
    "#" + PANEL + " .pkh-row{display:block;padding:9px 0;}",
    "#" + PANEL + " .pkh-row + .pkh-row{border-top:1px solid rgba(255,255,255,.07);}",
    "#" + PANEL + " .pkh-name{display:block;font:600 13.5px/1.35 system-ui,sans-serif;",
    "color:#f0f3f9;overflow-wrap:break-word;}",
    "#" + PANEL + " .pkh-chip{display:inline-block;padding:1px 7px;border-radius:999px;",
    "font:700 9.5px/15px system-ui,sans-serif;letter-spacing:.08em;margin-left:7px;",
    "vertical-align:1px;border:1px solid rgba(255,255,255,.12);color:#aab6cc;}",
    "#" + PANEL + " .pkh-chip[data-sys='DS']{color:#ff9a8f;border-color:rgba(238,59,43,.38);",
    "background:rgba(238,59,43,.10);}",
    "#" + PANEL + " .pkh-chip[data-sys='GBA']{color:#c4a9ff;border-color:rgba(143,109,214,.40);",
    "background:rgba(143,109,214,.12);}",
    "#" + PANEL + " .pkh-chip[data-sys='N64']{color:#7fe0c2;border-color:rgba(31,161,122,.40);",
    "background:rgba(31,161,122,.12);}",
    "#" + PANEL + " .pkh-chip[data-sys='GC']{color:#d6a9ff;border-color:rgba(180,124,224,.40);",
    "background:rgba(180,124,224,.12);}",
    "#" + PANEL + " .pkh-why{display:block;font:12.5px/1.45 system-ui,sans-serif;",
    "color:#8d99b0;margin-top:2px;}",
    "#" + PANEL + " .pkh-opt{display:block;font:12.5px/1.5 system-ui,sans-serif;",
    "color:#9aa5ba;padding:2px 0 4px;}",
    "#" + PANEL + " .pkh-no{display:block;font:12px/1.5 system-ui,sans-serif;",
    "color:#8d99b0;padding:4px 0;}",
    "#" + PANEL + " .pkh-no::before{content:'\\00d7';color:rgba(238,59,43,.9);",
    "font-weight:700;margin-right:7px;}",
    "#" + PANEL + " .pkh-no b{display:inline;font-weight:600;color:#ffd05a;}",
    "#" + PANEL + " .pkh-tip{display:block;font:11.5px/1.5 system-ui,sans-serif;",
    "color:#7f8ba2;margin-top:10px;padding-top:9px;",
    "border-top:1px solid rgba(255,255,255,.07);}",
    "#" + PANEL + " .pkh-foot{display:flex;align-items:center;gap:10px;",
    "padding:15px 22px;border-top:1px solid rgba(255,255,255,.09);",
    "background:linear-gradient(180deg,rgba(255,255,255,.02),rgba(0,0,0,.10));}",
    "#" + PANEL + " .pkh-btn{display:block;cursor:pointer;border-radius:10px;",
    "padding:12px 16px;font:600 13px/1 system-ui,sans-serif;border:1px solid transparent;",
    "transition:background .15s ease,border-color .15s ease;}",
    "#" + PANEL + " .pkh-btn:focus-visible{outline:3px solid rgba(255,203,5,.35);",
    "outline-offset:2px;}",
    "#" + PANEL + " .pkh-go{flex:1;text-align:center;",
    "background:linear-gradient(180deg,#ffd83d,#ffcb05);color:#1b1600;",
    "box-shadow:0 6px 18px rgba(255,203,5,.20),inset 0 1px 0 rgba(255,255,255,.45);}",
    "#" + PANEL + " .pkh-go:hover{background:linear-gradient(180deg,#ffe15c,#ffd21a);}",
    "#" + PANEL + " .pkh-go:active{background:linear-gradient(180deg,#ffd21a,#f0bf00);}",
    "#" + PANEL + " .pkh-arrow{display:inline-block;margin-left:8px;font-size:14px;",
    "line-height:1;vertical-align:-1px;opacity:.75;}",
    "#" + PANEL + " .pkh-close{background:rgba(255,255,255,.06);",
    "border-color:rgba(255,255,255,.12);color:#cfd8e8;}",
    "#" + PANEL + " .pkh-close:hover{background:rgba(255,255,255,.12);}",
    "@media (max-width:700px){",
    "#" + PANEL + " .pkh-card{max-height:92vh;}",
    "#" + PANEL + " .pkh-body{grid-template-columns:1fr;padding:0 18px;}",
    "#" + PANEL + " .pkh-col{padding:12px 0 14px;}",
    "#" + PANEL + " .pkh-col:first-child{padding-right:0;}",
    "#" + PANEL + " .pkh-col + .pkh-col{border-left:0;",
    "border-top:1px solid rgba(255,255,255,.09);padding-left:0;}",
    "#" + PANEL + " .pkh-head{padding:18px 18px 14px;}",
    "#" + PANEL + " .pkh-foot{padding:13px 18px;}}",
    "@media (prefers-reduced-motion:reduce){#" + PANEL + " .pkh-btn{transition:none;}}"
  ].join("");

  var REQUIRED = [
    ["HeartGold or SoulSilver", "DS", "Johto and Kanto, with their towns and music"],
    ["Platinum, Diamond or Pearl", "DS", "Sinnoh, with its extra buildings"],
    ["Black, White, Black 2 or White 2", "DS", "Every Pokémon's picture, plus the Gen 5 moves and cries"],
    ["Ranger: Guardian Signs", "DS", "Your trainer and the riding poses"],
    ["Emerald", "GBA", "Move animations, cries, Hoenn music and the battle simulator"]
  ];

  var WRONG = [
    ["FireRed, LeafGreen, Ruby or Sapphire", "Use Emerald instead"],
    ["Gold, Silver or Crystal", "Use HeartGold or SoulSilver instead"],
    ["Ranger or Shadows of Almia", "Use Guardian Signs instead"],
    ["Explorers of Time or Darkness", "Use Sky instead"],
    ["Pokémon XD", "Not used at all"]
  ];

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text) node.textContent = text;
    return node;
  }

  function svgEl(tag, attrs) {
    var node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    Object.keys(attrs).forEach(function (k) { node.setAttribute(k, attrs[k]); });
    return node;
  }

  function questionIcon() {
    var svg = svgEl("svg", {
      "class": "pkh-ico", viewBox: "0 0 24 24",
      "aria-hidden": "true", focusable: "false"
    });
    svg.appendChild(svgEl("circle", {
      cx: "12", cy: "12", r: "9.1", fill: "none",
      stroke: "currentColor", "stroke-width": "1.5"
    }));
    svg.appendChild(svgEl("path", {
      d: "M9.2 9.15a2.95 2.95 0 1 1 4.55 2.6c-.95.58-1.75 1.16-1.75 2.45",
      fill: "none", stroke: "currentColor", "stroke-width": "1.7",
      "stroke-linecap": "round", "stroke-linejoin": "round"
    }));
    svg.appendChild(svgEl("circle", {
      cx: "12", cy: "17.35", r: "1.15", fill: "currentColor"
    }));
    return svg;
  }

  function ballIcon() {
    var svg = svgEl("svg", {
      "class": "pkh-ball", viewBox: "0 0 100 100",
      "aria-hidden": "true", focusable: "false"
    });
    svg.appendChild(svgEl("circle", { cx: "50", cy: "50", r: "46", fill: "#f4f6fa" }));
    svg.appendChild(svgEl("path", { d: "M4 50a46 46 0 0 1 92 0Z", fill: "#ee3b2b" }));
    svg.appendChild(svgEl("circle", {
      cx: "50", cy: "50", r: "46", fill: "none",
      stroke: "#0d111b", "stroke-width": "6"
    }));
    svg.appendChild(svgEl("rect", {
      x: "4", y: "46", width: "92", height: "8", fill: "#0d111b"
    }));
    svg.appendChild(svgEl("circle", { cx: "50", cy: "50", r: "15", fill: "#0d111b" }));
    svg.appendChild(svgEl("circle", { cx: "50", cy: "50", r: "10", fill: "#f4f6fa" }));
    svg.appendChild(svgEl("circle", { cx: "50", cy: "50", r: "5", fill: "#0d111b" }));
    return svg;
  }

  function row(name, sys, why) {
    var r = el("div", "pkh-row");
    var n = el("span", "pkh-name", name);
    if (sys) {
      var chip = el("span", "pkh-chip", sys);
      chip.setAttribute("data-sys", sys);
      n.appendChild(chip);
    }
    r.appendChild(n);
    r.appendChild(el("span", "pkh-why", why));
    return r;
  }

  function build(onShut) {
    var back = el("div");
    back.id = PANEL;
    back.setAttribute("role", "dialog");
    back.setAttribute("aria-modal", "true");
    back.setAttribute("aria-labelledby", "pokem-files-title");

    var card = el("div", "pkh-card");
    var head = el("div", "pkh-head");
    head.appendChild(ballIcon());

    var titles = el("div", "pkh-titles");
    var title = el("span", "pkh-title", "Which files do you need?");
    title.id = "pokem-files-title";
    titles.appendChild(title);
    titles.appendChild(el("span", "pkh-sub",
      "Bring your own copies. They are read here and never uploaded."));
    head.appendChild(titles);

    var x = el("button", "pkh-x", "\u00d7");
    x.type = "button";
    x.setAttribute("aria-label", "Close");
    head.appendChild(x);
    card.appendChild(head);

    var body = el("div", "pkh-body");

    var left = el("div", "pkh-col");
    left.appendChild(el("span", "pkh-h", "What to bring"));
    REQUIRED.forEach(function (g) { left.appendChild(row(g[0], g[1], g[2])); });
    left.appendChild(el("span", "pkh-tip",
      "You only need one game from each line. It looks inside the file, so the name does not matter."));
    body.appendChild(left);

    var right = el("div", "pkh-col");
    right.appendChild(el("span", "pkh-h", "Extras, if you have them"));
    right.appendChild(el("span", "pkh-opt",
      "Explorers of Sky adds move effects. Stadium, Stadium 2 and Colosseum add music."));
    right.appendChild(el("span", "pkh-h", "These will not work"));
    WRONG.forEach(function (g) {
      var p = el("span", "pkh-no", g[0] + ": ");
      p.appendChild(el("b", null, g[1]));
      right.appendChild(p);
    });
    right.appendChild(el("span", "pkh-tip",
      "The game knows the file and still says no."));
    body.appendChild(right);
    card.appendChild(body);

    var foot = el("div", "pkh-foot");
    var go = el("button", "pkh-btn pkh-go");
    go.type = "button";
    go.appendChild(document.createTextNode("How to make your own copies"));
    go.appendChild(el("span", "pkh-arrow", "\u2197"));
    go.addEventListener("click", function () { window.open(HELP, "_blank"); });
    var close = el("button", "pkh-btn pkh-close", "Close");
    close.type = "button";
    foot.appendChild(go);
    foot.appendChild(close);
    card.appendChild(foot);

    back.appendChild(card);

    function onKey(e) {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        shut();
        return;
      }
      if (e.key !== "Tab") return;
      var f = back.querySelectorAll("button, [href]");
      if (!f.length) return;
      var first = f[0];
      var last = f[f.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }

    function shut() {
      document.removeEventListener("keydown", onKey, true);
      back.remove();
      if (onShut) onShut();
    }

    x.addEventListener("click", shut);
    close.addEventListener("click", shut);
    back.addEventListener("click", function (e) { if (e.target === back) shut(); });
    back.addEventListener("mousedown", function (e) { e.stopPropagation(); });
    document.addEventListener("keydown", onKey, true);

    return back;
  }

  function injectStyle() {
    if (document.getElementById(STYLE)) return;
    var st = document.createElement("style");
    st.id = STYLE;
    st.textContent = TRIGGER_CSS + PANEL_CSS;
    (document.head || document.documentElement).appendChild(st);
  }

  function openPanel(e) {
    if (e) { e.preventDefault(); e.stopPropagation(); }
    if (document.getElementById(PANEL)) return;
    injectStyle();

    var lastFocus = document.activeElement;
    // The game hides every body child except a whitelist while the gate is up, so the
    // panel has to live inside the gate to be seen.
    var host = document.getElementById("gate") || document.body;
    host.appendChild(build(function () {
      if (lastFocus && lastFocus.isConnected) lastFocus.focus();
    }));
    var x = document.querySelector("#" + PANEL + " .pkh-x");
    if (x) x.focus();
  }

  function trigger() {
    var a = document.createElement("a");
    a.className = "pkh-trigger";
    a.setAttribute("href", HELP);
    a.setAttribute("data-pokem-help", "1");
    a.setAttribute("aria-haspopup", "dialog");
    a.appendChild(questionIcon());
    a.appendChild(el("span", "pkh-label", "Which files do I need?"));
    a.addEventListener("click", openPanel);
    return a;
  }

  function ensure() {
    if (!HELP) return;
    injectStyle();

    var gate = document.getElementById("gate");
    var actions = gate && gate.querySelector(".actions");
    if (actions && actions.isConnected && !gate.querySelector("[data-pokem-help]")) {
      var wrap = document.createElement("div");
      wrap.setAttribute("style",
        "all:initial;display:flex;justify-content:center;margin-top:14px;");
      wrap.appendChild(trigger());
      actions.insertAdjacentElement("afterend", wrap);
    }

    var missing = document.querySelector(".missing-files");
    if (missing && !missing.querySelector("[data-pokem-help]")) {
      var inline = trigger();
      inline.style.marginLeft = "12px";
      missing.appendChild(inline);
    }
  }

  ensure();
  setInterval(ensure, 2000);
})();
"##;

/// The wiki answers what the game only half answers: where a species lives, what it
/// learns, what it becomes. The game already carries the national number of every
/// Pokémon it draws (`data-dex` on each portrait), so a small Poké Ball on the corner of
/// that portrait opens the species' page. A plain link is all it takes, because the game
/// window already sends any wiki address to the wiki window.
///
/// Three places, and a reason to skip a fourth. The party window's six icons are
/// `<button>`s, and a link inside one is nesting a control in a control, so they go
/// without. The focused card, the party bar and the Pokédex page take it.
///
/// The Pokédex only gets it on a species you have met: an unseen one is drawn as
/// `section.dexpage` with no portrait number, while a seen one is `article.dexpage.dx`,
/// so the selector is the spoiler guard rather than a separate check.
///
/// The game redraws all three, so a sweep puts the ball back after each redraw rather
/// than assuming it stays.
const WIKI_LINKS_JS: &str = r##"
(function () {
  if (window.__pkw) return;
  window.__pkw = 1;
  var WIKI = "https://pokem.onl/wiki#/pokemon/";
  var SIZE = 15;

  var css = document.createElement("style");
  css.textContent = [
    "a.pkwiki{position:absolute;z-index:2;width:" + SIZE + "px;height:" + SIZE + "px;",
    "border-radius:50%;box-sizing:border-box;cursor:pointer;opacity:.55;",
    "background:radial-gradient(circle,#fff 0 2.5px,#1c1c22 2.5px 3.5px,transparent 3.5px),",
    "linear-gradient(#e8423a 0 44%,#1c1c22 44% 56%,#fff 56%);",
    "box-shadow:0 0 0 1px rgba(0,0,0,.45),0 1px 2px rgba(0,0,0,.35);",
    "transition:opacity .12s ease,transform .12s ease;}",
    "a.pkwiki:hover,a.pkwiki:focus-visible{opacity:1;transform:scale(1.15);outline:none;}",
  ].join("");
  document.head.appendChild(css);

  function put(host, canvas, n) {
    if (!n) return;
    /* The portrait's corner is measured against its offset parent, so make sure that is
       the element the ball is appended to. */
    if (getComputedStyle(host).position === "static") host.style.position = "relative";
    var a = host.querySelector(":scope > a.pkwiki");
    if (!a) {
      a = document.createElement("a");
      a.className = "pkwiki";
      a.setAttribute("aria-label", "Open this Pokémon in the wiki");
      a.title = "Wiki";
      ["pointerdown", "mousedown", "click"].forEach(function (ev) {
        a.addEventListener(ev, function (e) { e.stopPropagation(); });
      });
      host.appendChild(a);
    }
    a.href = WIKI + n;
    a.style.left = (canvas.offsetLeft + canvas.offsetWidth - SIZE) + "px";
    a.style.top = (canvas.offsetTop + 1) + "px";
  }

  function sweep() {
    var mons = document.querySelectorAll("#party .mon[data-a]");
    for (var i = 0; i < mons.length; i++) {
      var c = mons[i].querySelector("canvas[data-dex]");
      if (c) put(mons[i], c, Number(c.dataset.dex));
    }
    var cards = document.querySelectorAll(".pfocus article.card");
    for (var j = 0; j < cards.length; j++) {
      var d = cards[j].querySelector("canvas[data-dex]");
      if (d) put(cards[j], d, Number(d.dataset.dex));
    }
    /* The dex only draws a real page once a species is seen: an unseen one is a section
       with no portrait number at all, so it never matches this. A form's page carries
       the form's own number, which is what the wiki wants anyway. */
    var heroes = document.querySelectorAll("article.dexpage.dx .dx-hero > canvas[data-spr]");
    for (var k = 0; k < heroes.length; k++) {
      var h = heroes[k].parentElement;
      if (h) put(h, heroes[k], Number(heroes[k].dataset.spr));
    }
  }

  var queued = false;
  function queue() {
    if (queued) return;
    queued = true;
    requestAnimationFrame(function () { queued = false; sweep(); });
  }

  new MutationObserver(queue).observe(document.body, { childList: true, subtree: true });
  window.addEventListener("resize", queue);
  sweep();
})();
"##;

/// Files that ship beside the binary: the splash, the offline page. A released build
/// keeps `assets/` next to the executable, while `cargo build` leaves it in the repo,
/// two levels up from `target/<profile>`.
fn asset_path(name: &str) -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [
                dir.join("assets").join(name),
                dir.join("../../assets").join(name),
            ] {
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name)
}

/// Chromium wants forward slashes and a third slash before a drive letter.
fn file_url(path: &std::path::Path) -> String {
    let path = path.display().to_string().replace('\\', "/");
    let prefix = if path.starts_with('/') { "file://" } else { "file:///" };
    format!("{prefix}{path}")
}

fn splash_url() -> String {
    file_url(&asset_path("splash/index.html"))
}

fn offline_url() -> String {
    file_url(&asset_path("offline.html"))
}

/// The game's own origin, and nothing that merely begins with its name.
fn is_game_url(url: &str) -> bool {
    match url.strip_prefix("https://pokem.onl") {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#'),
        None => false,
    }
}

/// Percent-encode anything a hash query cannot carry, so a place name cannot smuggle its
/// own `#` or `&` into the wiki's route.
fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// F1 lands on the map you are standing on. The wiki resolves the name to that map's own
/// page and falls back to its filtered places list when nothing matches, so the name on
/// its own is enough. With no map known, F1 opens the wiki itself.
fn wiki_url_for(map: Option<&str>) -> String {
    match map.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{WIKI_URL}#/place?q={}", encode_query(text)),
        None => WIKI_URL.to_string(),
    }
}

/// The wiki lives on the game's origin, so `is_game_url` would let it through. Loading it
/// in this window replaces the running game with no way back, so it goes to a browser.
fn is_wiki_url(url: &str) -> bool {
    match url.strip_prefix("https://pokem.onl/wiki") {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#'),
        None => false,
    }
}

/// The splash is the one screen that carries the update pill.
fn is_splash_url(url: &str) -> bool {
    let url = url.replace('\\', "/");
    url.starts_with("file://") && url.contains("/assets/splash/")
}

/// Our own pages are the only files this window is allowed to show.
fn is_launcher_page(url: &str) -> bool {
    let url = url.replace('\\', "/");
    url.starts_with("file://")
        && (url.contains("/assets/splash/") || url.contains("/assets/offline.html"))
}

/// Hand a link to the desktop's own browser. The window only ever shows the game, so
/// anything that is not the game belongs outside it.
fn open_external(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }

    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");

    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");

    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        // No console window should flash behind the launcher.
        command.creation_flags(0x0800_0000);
        command
    };

    command.arg(url);
    let _ = command.spawn();
}

/// `v0.1.0` and `0.1.0` both parse, and a suffix like `-rc1` is ignored.
fn version_tuple(tag: &str) -> Option<(u32, u32, u32)> {
    let text = tag.trim().trim_start_matches('v');
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(tag: &str) -> bool {
    match (version_tuple(tag), version_tuple(env!("CARGO_PKG_VERSION"))) {
        (Some(latest), Some(running)) => latest > running,
        _ => false,
    }
}

/// A tag is only ever shown, but it still gets there as text and as a script literal, so
/// anything that is not part of a version number is dropped.
fn safe_version(tag: &str) -> String {
    tag.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

fn update_cache_path(cache: &str) -> std::path::PathBuf {
    std::path::Path::new(cache).join("launcher-update.json")
}

fn read_update_cache(cache: &str) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(update_cache_path(cache)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((
        value.get("latest")?.as_str()?.to_owned(),
        value.get("checked")?.as_u64()?,
    ))
}

fn write_update_cache(cache: &str, latest: &str, checked: u64) {
    let body = serde_json::json!({ "latest": latest, "checked": checked }).to_string();
    let _ = std::fs::create_dir_all(cache);
    let _ = std::fs::write(update_cache_path(cache), body);
}

/// GitHub wants a user agent and answers with `tag_name`. curl is already on every
/// platform this ships to, so the launcher does not grow a TLS stack for one call a day.
fn fetch_latest_tag() -> Option<String> {
    let mut command = std::process::Command::new("curl");
    command.args([
        "-fsSL",
        "--max-time",
        "8",
        "-H",
        "User-Agent: pokem-online-launcher",
        RELEASE_API,
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // No console window behind the launcher.
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value.get("tag_name")?.as_str().map(str::to_owned)
}

/// Once a day, ask GitHub for the newest release. A failed check stays quiet: a missing
/// notice is not worth an error on anyone's screen.
fn spawn_update_check(cache: String) {
    std::thread::spawn(move || {
        let now = now_ms() / 1000;
        // POKEM_UPDATE_TAG pretends a release exists, so the notice can be looked at
        // without cutting one.
        let forced = std::env::var("POKEM_UPDATE_TAG").ok().filter(|tag| !tag.is_empty());
        let latest = match forced {
            Some(tag) => Some(tag),
            None => match read_update_cache(&cache) {
                Some((tag, checked)) if now.saturating_sub(checked) < UPDATE_INTERVAL_SECS => {
                    Some(tag)
                }
                _ => fetch_latest_tag().inspect(|tag| write_update_cache(&cache, tag, now)),
            },
        };
        let Some(latest) = latest else { return };
        if !is_newer(&latest) {
            return;
        }
        if debug_echo() {
            eprintln!("update available: {latest}");
        }
        if let Ok(mut slot) = UPDATE_READY.lock() {
            *slot = Some(latest.clone());
        }
        let mut task = ShowUpdateNotice::new(latest);
        post_task(ThreadId::UI, Some(&mut task));
    });
}

/// The pill belongs to the splash: its shape, its animation and its words all live in
/// that page. The launcher only says that a newer build exists, and where to read about it.
fn splash_notice_js(version: &str) -> String {
    format!(
        "window.__pokemShowPill && window.__pokemShowPill(\"{}\", \"{}\")",
        safe_version(version),
        RELEASE_PAGE
    )
}

/// Title first, because it outlives the splash and survives fullscreen being toggled.
fn apply_update_notice(version: &str) {
    let version = safe_version(version);
    if let Some(window) = WINDOW.lock().ok().and_then(|held| held.clone()) {
        if window.is_closed() == 0 {
            let title = CefString::from(format!("{APP_NAME} · {version} ready").as_str());
            window.set_title(Some(&title));
        }
    }
    let Some(browser) = BROWSER.lock().ok().and_then(|held| held.clone()) else {
        if debug_echo() {
            eprintln!("update notice: no browser yet");
        }
        return;
    };
    let Some(mut frame) = browser.main_frame() else {
        if debug_echo() {
            eprintln!("update notice: no main frame yet");
        }
        return;
    };
    if frame.is_main() != 1 {
        return;
    }
    let url = CefString::from(&frame.url()).to_string();
    if is_splash_url(&url) {
        if NOTICE_SHOWN.swap(true, Ordering::SeqCst) {
            return;
        }
        if debug_echo() {
            eprintln!("update notice: shown on the splash");
        }
        let script = CefString::from(splash_notice_js(&version).as_str());
        let source = CefString::from(url.as_str());
        frame.execute_java_script(Some(&script), Some(&source), 0);
    } else if is_game_url(&url) {
        // The game keeps its own design; the pill is ours and sits at the bottom of it.
        let state = update_state_json(&version);
        ensure_update_pill(&mut frame, &state);
    }
}

/// The in game pill. It lives in a shadow root so the game's own styles cannot reach it,
/// and it only exists when a newer build is known.
/// The in game pill. It is the splash's pill, unchanged: the same white glass, the same
/// bell, the same yellow dot. It lives in a shadow root so the game's own styles cannot
/// reach it, and it sits at the bottom of the screen over the game.
const UPDATE_PILL_JS: &str = r##"
(function () {
  if (window.__pokemPill) return;
  window.__pokemPill = true;
  var host = document.createElement("div");
  host.id = "pokem-update";
  host.style.cssText = "position:fixed;left:50%;top:18px;transform:translateX(-50%);z-index:2147483647;pointer-events:none;";
  var root = host.attachShadow({ mode: "open" });
  root.innerHTML = [
    "<style>",
    ":host{all:initial}",
    ".pill{--h:clamp(40px,4.4vmin,56px);--w:calc(var(--h)*6);pointer-events:auto;position:relative;display:block;height:var(--h);width:var(--h);transform:scale(0);border:1px solid rgba(255,255,255,.38);border-radius:999px;overflow:hidden;cursor:pointer;color:#fff;text-decoration:none;background:linear-gradient(135deg,rgba(255,255,255,.24),rgba(255,255,255,.08));-webkit-backdrop-filter:blur(16px) saturate(170%);backdrop-filter:blur(16px) saturate(170%);box-shadow:0 10px 28px rgba(0,0,0,.30),inset 0 1px 0 rgba(255,255,255,.5),inset 0 -1px 0 rgba(255,255,255,.08)}",
    ".pill:hover{background:linear-gradient(135deg,rgba(255,255,255,.32),rgba(255,255,255,.13))}",
    ".pill:focus-visible{outline:3px solid #fff;outline-offset:3px}",
    ".pill.play{animation:pill-pop .7s ease-in-out both,pill-stretch .85s cubic-bezier(.22,1.15,.36,1) .62s both}",
    ".inner{position:absolute;top:0;bottom:0;left:50%;width:var(--w);transform:translateX(-50%);display:grid;grid-template-columns:1fr var(--h) 1fr;align-items:center}",
    ".label{font:700 calc(var(--h)*.305)/1 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;letter-spacing:.01em;white-space:nowrap;opacity:0;text-shadow:0 1px 2px rgba(56,3,1,.6),0 0 10px rgba(56,3,1,.3)}",
    ".label.left{text-align:right;padding-right:calc(var(--h)*.10)}",
    ".label.right{text-align:left;padding-left:calc(var(--h)*.10)}",
    ".play .label.left{animation:from-right .45s ease-out .85s both}",
    ".play .label.right{animation:from-left .45s ease-out .85s both}",
    ".icon{position:relative;display:grid;place-items:center;height:var(--h)}",
    ".icon svg{width:calc(var(--h)*.45);height:calc(var(--h)*.45);filter:drop-shadow(0 1px 2px rgba(56,3,1,.45))}",
    ".play .icon svg{animation:pill-ring .7s ease-in-out 1.35s both}",
    ".dot{position:absolute;top:calc(var(--h)*.25);right:calc(var(--h)*.25);width:calc(var(--h)*.18);height:calc(var(--h)*.18);border-radius:50%;background:#ffcb05;box-shadow:0 0 0 2px rgba(255,255,255,.55)}",
    ".play .dot{animation:pill-dot .35s cubic-bezier(.3,1.6,.5,1) 1.25s both}",
    ".bar{position:absolute;left:0;bottom:0;height:3px;width:0;border-radius:0 3px 3px 0;background:linear-gradient(90deg,rgba(255,203,5,.42),rgba(255,203,5,.88));box-shadow:0 0 10px rgba(255,203,5,.45),inset 0 1px 0 rgba(255,255,255,.55);transition:width .25s ease}",
    "@keyframes pill-pop{0%{transform:scale(0)}50%{transform:scale(1.22)}75%{transform:scale(.93)}100%{transform:scale(1)}}",
    "@keyframes pill-stretch{from{width:var(--h)}to{width:var(--w)}}",
    "@keyframes from-right{from{opacity:0;transform:translateX(14px)}to{opacity:1;transform:none}}",
    "@keyframes from-left{from{opacity:0;transform:translateX(-14px)}to{opacity:1;transform:none}}",
    "@keyframes pill-ring{0%,100%{transform:rotate(0)}20%{transform:rotate(16deg)}40%{transform:rotate(-13deg)}60%{transform:rotate(8deg)}80%{transform:rotate(-4deg)}}",
    "@keyframes pill-dot{from{transform:scale(0)}to{transform:scale(1)}}",
    "@media (prefers-reduced-motion:reduce){.pill,.pill.play{animation:none;transform:none;width:var(--w)}.label{opacity:1;animation:none!important}.play .icon svg,.play .dot{animation:none}.bar{transition:none}}",
    "</style>",
    '<a class="pill" role="button" tabindex="0" aria-live="polite">',
    '<span class="inner">',
    '<span class="label left">Update</span>',
    '<span class="icon">',
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">',
    '<path d="M6 9a6 6 0 0 1 12 0c0 6 2.5 7.5 2.5 7.5h-17S6 15 6 9z"/>',
    '<path d="M10 20a2 2 0 0 0 4 0"/>',
    "</svg>",
    '<span class="dot"></span>',
    "</span>",
    '<span class="label right">Available</span>',
    "</span>",
    '<span class="bar"></span>',
    "</a>"
  ].join("");
  document.documentElement.appendChild(host);
  var pill = root.querySelector(".pill");
  // The splash's entrance: the circle pops, then stretches out and the words arrive.
  void pill.offsetWidth;
  pill.classList.add("play");
  var bar = root.querySelector(".bar");
  var right = root.querySelector(".label.right");
  var state = "available";
  var version = "";
  var release = "";

  function send(cmd) {
    console.log("PO " + JSON.stringify({ cmd: cmd }));
  }

  function paint(next) {
    if (!next) return;
    if (next.state) state = next.state;
    if (next.version) version = next.version;
    if (next.release) release = next.release;
    if (state === "available") {
      right.textContent = version || "Available";
      bar.style.width = "0";
      pill.title = "Launcher " + version + " is available";
      pill.setAttribute("aria-label", "Update the launcher to " + version);
    } else if (state === "downloading") {
      right.textContent = (next.percent || 0) + "%";
      bar.style.width = (next.percent || 0) + "%";
      pill.title = "Downloading launcher " + version;
      pill.setAttribute("aria-label", "Downloading update, " + (next.percent || 0) + " percent");
    } else if (state === "ready") {
      right.textContent = "Restart";
      bar.style.width = "100%";
      pill.title = "Launcher " + version + " is ready";
      pill.setAttribute("aria-label", "Restart to finish the update");
    } else if (state === "failed") {
      right.textContent = "Retry";
      bar.style.width = "0";
      pill.title = next.detail || "The update did not finish";
      pill.setAttribute("aria-label", "Update failed, try again");
    } else if (state === "unsupported") {
      right.textContent = "GitHub";
      bar.style.width = "0";
      pill.title = "This copy cannot update itself";
      pill.setAttribute("aria-label", "Open the release page");
    }
  }

  window.__pokemUpdate = paint;
  if (window.__pokemUpdateState) paint(window.__pokemUpdateState);

  pill.addEventListener("click", function () {
    if (state === "available" || state === "failed" || state === "downloading") {
      send("update");
    } else if (state === "ready") {
      send("restart");
    } else if (state === "unsupported") {
      window.open(release || "https://github.com/ArisSwift/Pokemon-Online-Launcher/releases/latest", "_blank", "noreferrer");
    }
  });
  pill.addEventListener("keydown", function (event) {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      pill.click();
    }
  });
})();
"##;

fn update_state_json(version: &str) -> String {
    let phase = UPDATE_PHASE
        .lock()
        .map(|held| held.clone())
        .unwrap_or(UpdatePhase::Idle);
    let (state, percent, detail) = match phase {
        UpdatePhase::Idle => ("available", 0, String::new()),
        UpdatePhase::Downloading(percent) => ("downloading", percent, String::new()),
        UpdatePhase::Ready => ("ready", 100, String::new()),
        UpdatePhase::Failed(reason) => ("failed", 0, reason),
        UpdatePhase::Unsupported => ("unsupported", 0, String::new()),
    };
    serde_json::json!({
        "state": state,
        "percent": percent,
        "version": safe_version(version),
        "detail": detail,
        "release": RELEASE_PAGE,
    })
    .to_string()
}

/// Put the pill into the game page and tell it where things stand. Both scripts are
/// queued in order, so the pill exists before the first paint.
fn ensure_update_pill(frame: &mut Frame, state: &str) {
    let url = CefString::from(&frame.url()).to_string();
    let source = CefString::from(url.as_str());
    let seed = CefString::from(format!("window.__pokemUpdateState = {state};").as_str());
    frame.execute_java_script(Some(&seed), Some(&source), 0);
    let pill = CefString::from(UPDATE_PILL_JS);
    frame.execute_java_script(Some(&pill), Some(&source), 0);
    let paint = CefString::from(
        format!("window.__pokemUpdate && window.__pokemUpdate({state});").as_str(),
    );
    frame.execute_java_script(Some(&paint), Some(&source), 0);
}

fn paint_update() {
    let Some(version) = UPDATE_READY.lock().ok().and_then(|held| held.clone()) else {
        return;
    };
    let Some(browser) = BROWSER.lock().ok().and_then(|held| held.clone()) else {
        return;
    };
    let Some(mut frame) = browser.main_frame() else { return };
    let url = CefString::from(&frame.url()).to_string();
    let state = update_state_json(&version);
    if is_splash_url(&url) {
        let script = CefString::from(
            format!("window.__pokemUpdate && window.__pokemUpdate({state});").as_str(),
        );
        let source = CefString::from(url.as_str());
        frame.execute_java_script(Some(&script), Some(&source), 0);
    } else if is_game_url(&url) {
        ensure_update_pill(&mut frame, &state);
    }
}

enum UpdateError {
    Unsupported,
    Failed(String),
}

/// Download and check the newer build, then leave it staged. Nothing is renamed until
/// the game is closing or the player asks for the restart, so the running copy is never
/// touched while a renderer might still be started from it.
fn download_update() -> Result<update::Target, UpdateError> {
    let target = update::target().ok_or(UpdateError::Unsupported)?;
    let progress = |percent: u8| set_phase(UpdatePhase::Downloading(percent));

    // A local file stands in for the network, so the whole path can be exercised
    // without spending data.
    if let Some(source) = std::env::var_os("POKEM_UPDATE_FILE") {
        update::copy(&std::path::PathBuf::from(source), &target.part, &progress)
            .map_err(UpdateError::Failed)?;
        update::unpack(&target).map_err(UpdateError::Failed)?;
        return Ok(target);
    }

    let release = update::fetch_release(RELEASE_API)
        .ok_or_else(|| UpdateError::Failed("the release list could not be read".to_owned()))?;
    let asset = release
        .asset(target.asset)
        .ok_or_else(|| UpdateError::Failed(format!("{} is not in the release", target.asset)))?;
    if debug_echo() {
        eprintln!("update: {} offers {} ({} bytes)", release.tag, asset.name, asset.size);
    }
    let checksums = release
        .asset("SHA256SUMS")
        .ok_or_else(|| UpdateError::Failed("the release has no SHA256SUMS".to_owned()))?;
    let text = update::fetch_text(&checksums.url)
        .ok_or_else(|| UpdateError::Failed("SHA256SUMS could not be read".to_owned()))?;
    let expected = update::parse_checksums(&text, target.asset)
        .ok_or_else(|| UpdateError::Failed("no checksum for this build".to_owned()))?;

    update::download(&asset.url, &target.part, asset.size, &progress)
        .map_err(UpdateError::Failed)?;
    let actual = update::sha256_file(&target.part)
        .map(|bytes| update::hex(&bytes))
        .map_err(|error| UpdateError::Failed(error.to_string()))?;
    if actual != expected {
        let _ = std::fs::remove_file(&target.part);
        return Err(UpdateError::Failed(
            "the download did not match its checksum".to_owned(),
        ));
    }
    update::record_hash(&target, &expected);
    update::unpack(&target).map_err(UpdateError::Failed)?;
    Ok(target)
}

fn start_update() {
    let known = UPDATE_READY.lock().map(|held| held.is_some()).unwrap_or(false);
    if !known {
        return;
    }
    if UPDATE_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    set_phase(UpdatePhase::Downloading(0));
    std::thread::spawn(move || {
        match download_update() {
            Ok(target) => {
                if let Ok(mut held) = UPDATE_STAGED.lock() {
                    *held = Some(target);
                }
                set_phase(UpdatePhase::Ready);
            }
            Err(UpdateError::Unsupported) => set_phase(UpdatePhase::Unsupported),
            Err(UpdateError::Failed(message)) => {
                if debug_echo() {
                    eprintln!("update failed: {message}");
                }
                set_phase(UpdatePhase::Failed(message));
            }
        }
        UPDATE_BUSY.store(false, Ordering::SeqCst);
    });
}

fn restart_now() {
    let Some(target) = UPDATE_STAGED.lock().ok().and_then(|held| held.clone()) else {
        return;
    };
    // A second click after a swap that already landed has nothing to move; it only needs
    // the window to close so the new build can start.
    if target.staging.exists() {
        if let Err(error) = update::install(&target) {
            if debug_echo() {
                eprintln!("update install failed: {error}");
            }
            set_phase(UpdatePhase::Failed(error));
            return;
        }
        update::mark_backup(&target);
    }
    if let Err(error) = update::spawn(&target) {
        if debug_echo() {
            eprintln!("could not start the new build: {error}");
        }
        set_phase(UpdatePhase::Failed(error));
        return;
    }
    // Quitting the message loop from inside a console callback leaves CEF running here.
    // Closing the game window does work: its own client ends the loop on the way out.
    let closed = BROWSER
        .lock()
        .ok()
        .and_then(|held| held.clone())
        .and_then(|browser| browser.host())
        .map(|host| host.try_close_browser())
        .unwrap_or(0);
    if closed == 0 {
        if debug_echo() {
            eprintln!("the window did not close, quitting the loop instead");
        }
        quit_message_loop();
    }
}

/// A checked copy that was left behind by a quit still belongs on disk. The next start
/// puts it in place before CEF loads anything.
fn install_on_quit() {
    let Some(target) = UPDATE_STAGED.lock().ok().and_then(|held| held.clone()) else {
        return;
    };
    if update::install(&target).is_ok() {
        update::mark_backup(&target);
    }
}

fn startup_update() {
    let Some(target) = update::target() else { return };
    update::finish_previous(&target);
    if !target.staging.exists() {
        return;
    }
    let staged_ok = if target.kind == update::Kind::AppImage {
        match update::staged_hash(&target) {
            Some(expected) => update::sha256_file(&target.staging)
                .map(|bytes| update::hex(&bytes) == expected)
                .unwrap_or(false),
            None => true,
        }
    } else {
        true
    };
    if staged_ok && update::install(&target).is_ok() {
        update::mark_backup(&target);
        if debug_echo() {
            eprintln!("installed the staged update on start");
        }
    } else {
        update::remove_any(&target.staging);
        let _ = std::fs::remove_file(update::with_suffix(&target.staging, ".sha256"));
    }
}

fn main() {
    // macOS ships CEF as a framework that has to be loaded by hand before any other CEF
    // call. The main app finds it beside the executable; helper processes sit deeper in
    // the bundle and resolve it from there.
    #[cfg(target_os = "macos")]
    let _cef_loader = {
        let helper = std::env::args().any(|arg| arg == "--type" || arg.starts_with("--type="));
        let exe = std::env::current_exe().expect("current executable");
        cef::library_loader::LibraryLoader::new(&exe, helper)
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return;
    };
    run_main(args.as_main_args(), &cmd_line);
}

fn run_main(main_args: &MainArgs, cmd_line: &CommandLine) {
    let type_switch = CefString::from("type");
    let is_browser_process = cmd_line.has_switch(Some(&type_switch)) != 1;

    let ret = execute_process(Some(main_args), None, std::ptr::null_mut());
    if !is_browser_process {
        return;
    }
    assert_eq!(ret, -1, "browser process should not initialize CEF");

    if let Some(cmd) = command_line_get_global() {
        let name = CefString::from("autoplay-policy");
        let value = CefString::from("no-user-gesture-required");
        cmd.append_switch_with_value(Some(&name), Some(&value));

        // A covered window otherwise loses its timers, and with them its place online;
        // the game expects to keep playing while the player looks at something else.
        let occluded = CefString::from("disable-backgrounding-occluded-windows");
        cmd.append_switch(Some(&occluded));

        // On a machine with two GPUs Chromium may pick the integrated one and leave the
        // game on it.
        let gpu = CefString::from("force_high_performance_gpu");
        cmd.append_switch(Some(&gpu));
    }

    let cache = cache_dir();

    // One window, one profile. A second launch asks the running instance to come forward
    // and stops here, before CEF is initialized, so two builds cannot share a profile.
    let mut instance = one_instance::acquire(&instance_socket());
    // A restart after an update starts while the old process is still letting go of the
    // socket. Give it a few seconds instead of mistaking it for a second copy.
    if instance.is_none() && std::env::var_os("POKEM_WAIT_INSTANCE").is_some() {
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            instance = one_instance::acquire(&instance_socket());
            if instance.is_some() {
                break;
            }
        }
    }
    if instance.is_none() {
        eprintln!("pokem-online is already running, bringing it to the front");
        return;
    }
    // An older build holds the profile without holding our socket.
    if let Some(pid) = profile_holder(&cache) {
        eprintln!("pokem-online is already running (pid {pid})");
        return;
    }
    clear_stale_singleton(&cache);

    // A checked copy from the last session goes in before CEF starts, so a quit that
    // was cut short still lands the update.
    startup_update();

    spawn_presence();

    // POKEM_UA is only for testing what the site does with a different agent.
    let user_agent = std::env::var("POKEM_UA").unwrap_or_else(|_| user_agent());
    // A devtools protocol port, for driving the page from outside. Loopback only, and
    // only when it is asked for: a debug port is a hole in the browser.
    let debug_port = std::env::var("POKEM_DEBUG_PORT")
        .ok()
        .and_then(|p| p.parse::<i32>().ok())
        .unwrap_or(0);
    let settings = Settings {
        no_sandbox: 1,
        cache_path: CefString::from(cache.as_str()),
        user_agent: CefString::from(user_agent.as_str()),
        remote_debugging_port: debug_port,
        ..Default::default()
    };

    let mut app = LauncherApp::new();
    assert_eq!(
        initialize(Some(main_args), Some(&settings), Some(&mut app), std::ptr::null_mut()),
        1,
        "CEF failed to initialize"
    );

    // Now there is a UI thread for the raise task to run on.
    if let Some(instance) = instance.as_mut() {
        instance.listen();
    }

    spawn_update_check(cache.clone());

    run_message_loop();
    shutdown();
}

fn cache_dir() -> String {
    cache_base().join("pokem-online").to_string_lossy().into_owned()
}

#[cfg(target_os = "windows")]
fn cache_base() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

#[cfg(target_os = "macos")]
fn cache_base() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|home| std::path::PathBuf::from(home).join("Library").join("Caches"))
        .unwrap_or_else(|_| std::env::temp_dir())
}

#[cfg(target_os = "linux")]
fn cache_base() -> std::path::PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Where the running instance answers. It lives in the cache root rather than inside the
/// profile, because Chromium clears its own lock files in there.
fn instance_socket() -> std::path::PathBuf {
    cache_base().join("pokem-online.sock")
}

/// One launcher per user, whatever build it came from.
///
/// Chromium's own profile lock is not enough: it panics the process instead of saying
/// anything useful, and two builds that reach CEF at the same moment can both get in. The
/// first instance holds a socket or a mutex instead, and a second launch asks it to come
/// forward and stops before CEF is initialized.
#[cfg(unix)]
mod one_instance {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    use super::{post_task, ThreadId};

    /// What a second launch sends. The payload is never read; the connection is the
    /// request.
    const RAISE: &[u8] = b"raise\n";

    pub struct Guard {
        listener: Option<UnixListener>,
        path: PathBuf,
    }

    /// Bind the socket, or poke whichever instance already holds it and return None.
    pub fn acquire(path: &Path) -> Option<Guard> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(listener) = UnixListener::bind(path) {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            return Some(Guard { listener: Some(listener), path: path.to_path_buf() });
        }
        if let Ok(mut stream) = UnixStream::connect(path) {
            let _ = stream.write_all(RAISE);
            return None;
        }
        // Nobody answered, so the file is left over from a crash.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).ok()?;
        Some(Guard { listener: Some(listener), path: path.to_path_buf() })
    }

    impl Guard {
        /// Start answering. Called once CEF is up, because raising the window is a task
        /// for the UI thread and there is no UI thread before that.
        pub fn listen(&mut self) {
            let Some(listener) = self.listener.take() else { return };
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stream.is_err() {
                        continue;
                    }
                    let mut task = super::RaiseWindow::new();
                    post_task(ThreadId::UI, Some(&mut task));
                }
            });
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod one_instance {
    use std::ffi::c_void;
    use std::path::Path;

    const ERROR_ALREADY_EXISTS: u32 = 183;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct Guard {
        handle: *mut c_void,
    }

    /// A named kernel mutex. Windows releases it when the process ends, so a crash cannot
    /// leave a lock behind, and there is nothing to poke, so a second launch just leaves.
    pub fn acquire(_path: &Path) -> Option<Guard> {
        let name: Vec<u16> = "Local\\pokem-online-launcher\0".encode_utf16().collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            // Without a lock the launcher should still start rather than refuse forever.
            return Some(Guard { handle });
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Guard { handle })
    }

    impl Guard {
        pub fn listen(&mut self) {}
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

/// The pid in Chromium's `<host>-<pid>` lock link.
#[cfg(target_os = "linux")]
fn singleton_pid(cache: &str) -> Option<u32> {
    std::fs::read_link(std::path::Path::new(cache).join("SingletonLock"))
        .ok()?
        .to_string_lossy()
        .rsplit('-')
        .next()?
        .parse()
        .ok()
}

/// Whether that pid is still a launcher and not just a recycled number.
#[cfg(target_os = "linux")]
fn launcher_alive(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg.ends_with(b"pokem-online"))
}

/// A launcher that already owns the profile without holding our socket, which is how an
/// older build looks.
#[cfg(target_os = "linux")]
fn profile_holder(cache: &str) -> Option<u32> {
    singleton_pid(cache).filter(|pid| launcher_alive(*pid))
}

#[cfg(not(target_os = "linux"))]
fn profile_holder(_cache: &str) -> Option<u32> {
    None
}

/// Chromium leaves a SingletonLock symlink (`<host>-<pid>`) behind when the browser
/// process dies without a clean shutdown, and on this build it keeps refusing the
/// profile afterwards instead of noticing the pid is gone. Clear it once we know no live
/// launcher owns the profile.
#[cfg(target_os = "linux")]
fn clear_stale_singleton(cache: &str) {
    if singleton_pid(cache).is_none() {
        return;
    }
    let root = std::path::Path::new(cache);
    for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = std::fs::remove_file(root.join(name));
    }
}

#[cfg(not(target_os = "linux"))]
fn clear_stale_singleton(_cache: &str) {}

/// The size the window had last time, and whether it was maximised. The position is
/// deliberately not kept: a screen that has been unplugged since cannot strand it.
#[derive(Clone, Copy)]
struct Shape {
    width: i32,
    height: i32,
    maximized: bool,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            maximized: false,
        }
    }
}

fn shape_file() -> std::path::PathBuf {
    std::path::PathBuf::from(cache_dir()).join("window.json")
}

fn read_shape() -> Shape {
    let mut shape = Shape::default();
    let Ok(text) = std::fs::read_to_string(shape_file()) else {
        return shape;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return shape;
    };
    let number = |key: &str| value.get(key).and_then(serde_json::Value::as_i64);
    if let Some(width) = number("width") {
        shape.width = (width as i32).clamp(640, 10_000);
    }
    if let Some(height) = number("height") {
        shape.height = (height as i32).clamp(400, 10_000);
    }
    shape.maximized = value
        .get("maximized")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    shape
}

/// A maximised window reports the maximised bounds, so keep the last normal size and
/// only record the flag while it is maximised.
fn keep_shape(window: &mut Window) {
    let mut shape = read_shape();
    shape.maximized = window.is_maximized() == 1;
    if !shape.maximized {
        let bounds = window.bounds();
        shape.width = bounds.width;
        shape.height = bounds.height;
    }
    let text = format!(
        "{{\"width\":{},\"height\":{},\"maximized\":{}}}",
        shape.width, shape.height, shape.maximized
    );
    let _ = std::fs::write(shape_file(), text);
}

/// The wiki window's shape, kept in its own file so it never fights the game's.
struct WikiShape {
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    /// False until the window has been placed once, so the first open centres itself
    /// instead of landing in a corner.
    placed: bool,
    maximized: bool,
}

impl Default for WikiShape {
    fn default() -> Self {
        Self {
            width: 900,
            height: 900,
            x: 0,
            y: 0,
            placed: false,
            maximized: false,
        }
    }
}

fn wiki_shape_file() -> std::path::PathBuf {
    std::path::PathBuf::from(cache_dir()).join("wiki-window.json")
}

fn read_wiki_shape() -> WikiShape {
    let mut shape = WikiShape::default();
    let Ok(text) = std::fs::read_to_string(wiki_shape_file()) else {
        return shape;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return shape;
    };
    let number = |key: &str| value.get(key).and_then(serde_json::Value::as_i64);
    if let Some(width) = number("width") {
        shape.width = (width as i32).clamp(480, 10_000);
    }
    if let Some(height) = number("height") {
        shape.height = (height as i32).clamp(400, 10_000);
    }
    if let (Some(x), Some(y)) = (number("x"), number("y")) {
        // Wayland does not tell a client where it was placed, so a saved 0,0 is the
        // compositor's business, not a position to restore.
        if x != 0 || y != 0 {
            shape.x = x as i32;
            shape.y = y as i32;
            shape.placed = true;
        }
    }
    shape.maximized = value
        .get("maximized")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    shape
}

/// A maximised window reports the maximised bounds, so keep the last normal size and
/// only record the flag while it is maximised.
fn keep_wiki_shape(window: &mut Window) {
    let mut shape = read_wiki_shape();
    shape.maximized = window.is_maximized() == 1;
    if !shape.maximized {
        let bounds = window.bounds();
        shape.width = bounds.width.clamp(480, 10_000);
        shape.height = bounds.height.clamp(400, 10_000);
        // Only a position the window actually has is worth keeping. On Wayland the
        // client never learns where the compositor put it, so it reads back as 0,0.
        if bounds.x != 0 || bounds.y != 0 {
            shape.x = bounds.x;
            shape.y = bounds.y;
            shape.placed = true;
        }
    }
    let mut value = serde_json::json!({
        "width": shape.width,
        "height": shape.height,
        "maximized": shape.maximized,
    });
    if shape.placed {
        value["x"] = serde_json::json!(shape.x);
        value["y"] = serde_json::json!(shape.y);
    }
    let _ = std::fs::write(wiki_shape_file(), value.to_string());
}

/// The game names its maps `world:<slug>` where the slug is the area itself, so the
/// region has to come from the slug. Route numbers are unique across generations, so
/// those are exact; towns are matched by name.
fn region_name(region: &str) -> Option<&'static str> {
    let id = region.strip_prefix("world:").unwrap_or(region);
    let id = id.split(['#', '/']).next().unwrap_or(id).to_ascii_lowercase();

    if let Some(rest) = id.strip_prefix("route") {
        let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
        if let Ok(number) = digits.parse::<u32>() {
            return match number {
                1..=25 => Some("Kanto"),
                29..=46 => Some("Johto"),
                101..=134 => Some("Hoenn"),
                201..=230 => Some("Sinnoh"),
                _ => None,
            };
        }
    }

    const KANTO: &[&str] = &[
        "kanto", "pallet", "pallet-town", "viridian", "viridian-city", "pewter",
        "pewter-city", "cerulean", "cerulean-city", "vermilion", "vermilion-city",
        "celadon", "celadon-city", "lavender", "lavender-town", "fuchsia", "fuchsia-city",
        "saffron", "saffron-city", "cinnabar", "cinnabar-island", "indigo", "indigo-plateau",
        "rock-tunnel", "cave-rock-tunnel", "cave-digletts", "cave-mt-moon", "cave-moon-crags",
        "cave-cinnabar-gym", "cave-lavender-catacomb", "gym-celadon", "gym-fuchsia",
        "gym-saffron", "gym-pewter", "gym-cerulean", "gym-vermilion", "gym-viridian",
    ];
    const JOHTO: &[&str] = &[
        "johto", "new-bark", "cherrygrove", "violet", "violet-city", "azalea",
        "azalea-town", "goldenrod", "goldenrod-city", "ecruteak", "ecruteak-city", "olivine",
        "olivine-city", "cianwood", "cianwood-city", "mahogany", "mahogany-town", "blackthorn",
        "blackthorn-city", "mt-silver", "cave-mt-silver", "cave-mt-silver-dng-2",
        "cave-mt-silver-dng-3", "cave-slowpoke-well", "cave-union", "cave-ice-path",
        "cave-dragon-holy-land", "cave-silver-mine", "cave-silvers-summit", "cave-rage-depths",
        "gym-blackthorn", "gym-violet", "gym-azalea", "gym-goldenrod", "gym-ecruteak",
        "gym-olivine", "gym-cianwood", "gym-mahogany",
    ];
    const SEVII: &[&str] = &[
        "sevii", "sevii-knot", "sevii-quest", "sevii-tanoby", "sevii-one-island",
        "sevii-two-island", "sevii-three-island", "sevii-four-island", "sevii-five-island",
        "sevii-six-island", "sevii-seven-island", "cave-tanoby-chambers", "southern-island",
        "one-island", "two-island", "three-island", "four-island", "five-island",
        "six-island", "seven-island", "knot-island", "boon-island", "kin-island",
        "mt-ember", "cave-icefall", "cave-lost-cave", "cave-monean", "cave-dotted-hole",
        "cave-pattern-bush", "cave-altering-cave", "cave-outcast-island", "cave-green-path",
        "cave-water-path", "cave-ruin-valley", "cave-tanoby-key", "cave-five-isle-meadow",
        "cave-memorial-pillar", "cave-three-isle-port", "cave-resort-gorgeous",
        "cave-rocket-warehouse",
    ];
    const HOENN: &[&str] = &[
        "hoenn", "littleroot", "littleroot-town", "rustboro", "rustboro-city", "mauville",
        "mauville-city", "fallarbor", "fallarbor-town", "fortree", "fortree-city",
        "lilycove", "lilycove-city", "mossdeep", "mossdeep-city", "pacifidlog",
        "pacifidlog-town", "sootopolis", "sootopolis-city", "slateport", "slateport-city",
        "ever-grande", "ever-grande-city", "chimney", "mt-chimney", "cave-new-mauville",
        "cave-sky-pillar", "gym-fortree", "gym-mossdeep", "gym-rustboro", "gym-mauville",
        "gym-lavaridge", "gym-petalburg", "gym-dewford", "gym-sootopolis",
    ];
    const SINNOH: &[&str] = &[
        "sinnoh", "twinleaf", "twinleaf-town", "jubilife", "jubilife-city", "oreburgh",
        "oreburgh-city", "eterna", "eterna-city", "canalave", "canalave-city",
        "gym-oreburgh", "gym-eterna", "gym-jubilife",
    ];

    let id = id.as_str();
    if KANTO.contains(&id) {
        Some("Kanto")
    } else if JOHTO.contains(&id) {
        Some("Johto")
    } else if SEVII.contains(&id) {
        Some("Sevii Islands")
    } else if HOENN.contains(&id) {
        Some("Hoenn")
    } else if SINNOH.contains(&id) {
        Some("Sinnoh")
    } else {
        None
    }
}

/// `world:new-bark` reads as "New Bark", which is the game's own area slug. Used when
/// the page has not filled `#where` yet.
fn place_from_slug(region: &str) -> Option<String> {
    let id = region.strip_prefix("world:").unwrap_or(region);
    let id = id.split('#').next().unwrap_or(id);
    if id.is_empty() {
        return None;
    }
    let mut out = String::new();
    for word in id.split(['-', '_']).filter(|w| !w.is_empty() && !w.eq_ignore_ascii_case("cave")) {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The game's own words for what the player is doing, from its `/api/presence` beats.
/// `solo` and `online` only describe whether you share a channel, so the only one worth
/// putting on the card is the Battle Tower.
fn activity_word(activity: &str) -> Option<&'static str> {
    match activity {
        "tower" => Some("Battle Tower"),
        _ => None,
    }
}

/// Discord rejects details and state over 128 characters, and a stray long place name
/// would drop the whole card, so trim on a character boundary.
fn clip(text: &str) -> String {
    const LIMIT: usize = 120;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    text.chars().take(LIMIT - 1).collect::<String>() + "…"
}

fn presence_lines(snapshot: &Snapshot) -> (String, String, String) {
    let region = snapshot.region.as_deref().and_then(region_name);
    let place = snapshot
        .place
        .clone()
        .or_else(|| snapshot.region.as_deref().and_then(place_from_slug))
        .filter(|p| Some(p.as_str()) != region);
    // Solo is the default and says nothing; the Battle Tower is worth showing.
    let doing = snapshot.activity.as_deref().and_then(activity_word);

    let details = snapshot.name.clone().unwrap_or_else(|| APP_NAME.to_string());

    // "Pallet Town · Kanto" reads as where you are; fall back to whichever half exists.
    let mut state = match (&place, region) {
        (Some(place), Some(region)) => format!("{place} · {region}"),
        (Some(place), None) => place.clone(),
        (None, Some(region)) => region.to_string(),
        (None, None) => "Starting up".to_string(),
    };
    if let Some(doing) = doing {
        state.push_str(" · ");
        state.push_str(doing);
    }

    (clip(&details), clip(&state), region.unwrap_or(APP_NAME).to_string())
}

fn spawn_presence() {
    let (tx, rx) = mpsc::channel::<Snapshot>();
    let initial = tx.clone();
    let _ = PRESENCE_TX.set(tx);

    std::thread::spawn(move || {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut client: Option<DiscordIpcClient> = None;

        // Discord shows the card as soon as it gets an activity, so the first one goes
        // out before the page has reported anything: the splash is already on screen.
        let _ = initial.send(Snapshot::default());

        // A quiet channel just means the page has not reported in a while: wait for the
        // next snapshot rather than ending the thread, which would kill presence for good.
        loop {
            let mut snapshot = match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(snapshot) => snapshot,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            // A newer snapshot may already be queued; only the last one matters.
            while let Ok(newer) = rx.try_recv() {
                snapshot = newer;
            }

            if client.is_none() {
                let mut fresh = DiscordIpcClient::new(APP_ID);
                if fresh.connect().is_ok() {
                    client = Some(fresh);
                }
            }
            let Some(ipc) = client.as_mut() else {
                continue;
            };

            let (details, state, large_text) = presence_lines(&snapshot);

            let activity = activity::Activity::new()
                .details(details)
                .state(state)
                .timestamps(activity::Timestamps::new().start(started))
                .assets(
                    activity::Assets::new()
                        .large_image("logo")
                        .large_text(large_text),
                )
                .buttons(vec![activity::Button::new("Join", RELEASE_PAGE)]);

            // A dropped socket (Discord restarting) shows up here; reconnect on the next
            // snapshot rather than spinning.
            if ipc.set_activity(activity).is_err() {
                client = None;
            }
        }
    });
}

wrap_app! {
    struct LauncherApp {}

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(LauncherProcessHandler::new(RefCell::new(None)))
        }
    }
}

wrap_browser_process_handler! {
    struct LauncherProcessHandler {
        client: RefCell<Option<Client>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            *self.client.borrow_mut() = Some(LauncherClient::new());
            let mut client = self.client.borrow().clone();
            let mut settings = BrowserSettings::default();
            settings.background_color = 0xff_ee_3b_2b;
            // The hover bubble in the corner is a browser affordance: the launcher is a
            // window on the game, not a browser, and the game never needs it.
            settings.chrome_status_bubble = State::DISABLED;

            let url = CefString::from(splash_url().as_str());
            let mut view_delegate = LauncherBrowserViewDelegate::new(RuntimeStyle::DEFAULT);
            let browser_view = browser_view_create(
                client.as_mut(),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut view_delegate),
            );

            let shape = read_shape();
            let show_state = if shape.maximized {
                ShowState::MAXIMIZED
            } else {
                ShowState::NORMAL
            };

            let mut window_delegate = LauncherWindowDelegate::new(
                RefCell::new(browser_view),
                RuntimeStyle::DEFAULT,
                show_state,
                Size {
                    width: shape.width,
                    height: shape.height,
                },
                true,
            );
            window_create_top_level(Some(&mut window_delegate));

            // POKEM_WIKI opens the wiki beside the game, so the second window can be
            // checked from a terminal instead of by pressing F1.
            if std::env::var_os("POKEM_WIKI").is_some() {
                if debug_echo() {
                    eprintln!("POKEM_WIKI set, queueing the wiki window");
                }
                let mut task = ShowWiki::new(WIKI_URL.to_string());
                post_task(ThreadId::UI, Some(&mut task));
            }
        }
    }
}

wrap_window_delegate! {
    struct LauncherWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        runtime_style: RuntimeStyle,
        initial_show_state: ShowState,
        size: Size,
        persist: bool,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            self.size.clone()
        }
    }

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let browser_view = self.browser_view.borrow();
            let (Some(window), Some(browser_view)) = (window, browser_view.as_ref()) else {
                return;
            };
            // The browser view has to follow every resize, so let the window lay it out
            // as a fill. Without a layout the page keeps its old size inside a window
            // that has grown, which is what fullscreen looked like.
            window.set_to_fill_layout();
            let mut view = View::from(browser_view);
            window.add_child_view(Some(&mut view));
            let title = CefString::from(APP_NAME);
            window.set_title(Some(&title));
            if let Some(mut icon) = load_icon() {
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }
            if debug_echo() {
                let bounds = window.bounds();
                eprintln!("window {}x{}", bounds.width, bounds.height);
            }
            // High priority so the game's own key handling cannot swallow them.
            window.set_accelerator(CMD_FULLSCREEN, KEY_F11, 0, 0, 0, 1);
            window.set_accelerator(CMD_RELOAD, KEY_F5, 0, 0, 0, 1);
            // F1 is free in the game, so it carries the wiki.
            window.set_accelerator(CMD_WIKI, KEY_F1, 0, 0, 0, 1);
            // End, because M opens the map and Ctrl+M is easy to miss mid fight.
            window.set_accelerator(CMD_MUTE, KEY_END, 0, 0, 0, 1);
            if let Some(browser) = browser_view.browser() {
                if let Ok(mut held) = BROWSER.lock() {
                    *held = Some(browser);
                }
            }
            if self.initial_show_state != ShowState::HIDDEN {
                window.show();
            }
            // Held for the raise task, which is how a second launch finds this window.
            if self.persist {
                if let Ok(mut held) = WINDOW.lock() {
                    *held = Some(window.clone());
                }
            }
            // The check can land before the window exists; then this is the first chance.
            if let Some(version) = UPDATE_READY.lock().ok().and_then(|held| held.clone()) {
                apply_update_notice(&version);
            }
        }

        fn on_window_closing(&self, window: Option<&mut Window>) {
            // Only the game's own window owns the saved shape; a stray popup must not
            // overwrite it.
            if let (true, Some(window)) = (self.persist, window) {
                keep_shape(window);
            }
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
            if let Ok(mut held) = BROWSER.lock() {
                *held = None;
            }
            if self.persist {
                if let Ok(mut held) = WINDOW.lock() {
                    *held = None;
                }
            }
        }

        fn on_accelerator(&self, window: Option<&mut Window>, command_id: i32) -> i32 {
            let Some(window) = window else { return 0 };
            match command_id {
                CMD_FULLSCREEN => {
                    let fullscreen = window.is_fullscreen() == 1;
                    window.set_fullscreen((!fullscreen) as i32);
                    1
                }
                CMD_RELOAD => {
                    if let Some(browser_view) = self.browser_view.borrow().as_ref() {
                        if let Some(browser) = browser_view.browser() {
                            browser.reload();
                        }
                    }
                    1
                }
                CMD_WIKI => {
                    // Its own window, in this process, so it opens at once and the game
                    // keeps running behind it.
                    let map = WHERE.lock().ok().and_then(|held| held.clone());
                    open_wiki(&wiki_url_for(map.as_deref()));
                    1
                }
                CMD_MUTE => {
                    toggle_mute();
                    1
                }
                _ => 0,
            }
        }

        // CEF's C++ defaults answer true here, but a Rust delegate that leaves these
        // alone answers false, and a window that cannot be resized or maximised is
        // exactly what that looks like.
        fn can_resize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_maximize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn initial_show_state(&self, _window: Option<&mut Window>) -> ShowState {
            self.initial_show_state
        }

        fn window_runtime_style(&self) -> RuntimeStyle {
            self.runtime_style
        }
    }
}

wrap_browser_view_delegate! {
    struct LauncherBrowserViewDelegate {
        runtime_style: RuntimeStyle,
    }

    impl ViewDelegate {}

    impl BrowserViewDelegate {
        fn on_popup_browser_view_created(
            &self,
            _browser_view: Option<&mut BrowserView>,
            popup_browser_view: Option<&mut BrowserView>,
            _is_devtools: i32,
        ) -> i32 {
            let mut window_delegate = LauncherWindowDelegate::new(
                RefCell::new(popup_browser_view.cloned()),
                self.runtime_style,
                ShowState::NORMAL,
                Size {
                    width: 1024,
                    height: 768,
                },
                false,
            );
            window_create_top_level(Some(&mut window_delegate));
            1
        }

        fn browser_runtime_style(&self) -> RuntimeStyle {
            self.runtime_style
        }
    }
}

/// Open the wiki in a window of its own. It runs in this process, so it costs a fraction
/// of a second instead of starting the player's browser, and the game keeps running
/// behind it.
fn open_wiki(url: &str) {
    if debug_echo() {
        eprintln!("open_wiki: {url}");
    }
    // Already open: bring it forward and point it at the map you are on now.
    let existing = match WIKI_WINDOW.lock() {
        Ok(mut held) => {
            // A window the compositor closed without telling us is not worth showing.
            if held.as_ref().map(|window| window.is_closed() != 0).unwrap_or(false) {
                *held = None;
                if let Ok(mut browser) = WIKI_BROWSER.lock() {
                    *browser = None;
                }
            }
            held.clone()
        }
        Err(_) => None,
    };
    if let Some(window) = existing {
        if let Ok(browser) = WIKI_BROWSER.lock() {
            if let Some(browser) = browser.as_ref() {
                if let Some(frame) = browser.main_frame() {
                    let url = CefString::from(url);
                    frame.load_url(Some(&url));
                }
            }
        }
        window.show();
        window.activate();
        window.bring_to_top();
        return;
    }

    let mut client = WikiClient::new();
    let mut settings = BrowserSettings::default();
    settings.background_color = 0xff_ff_ff_ff;
    settings.chrome_status_bubble = State::DISABLED;
    let url = CefString::from(url);
    let mut view_delegate = WikiBrowserViewDelegate::new(RuntimeStyle::DEFAULT);
    let browser_view = browser_view_create(
        Some(&mut client),
        Some(&url),
        Some(&settings),
        None,
        None,
        Some(&mut view_delegate),
    );
    let mut window_delegate =
        WikiWindowDelegate::new(RefCell::new(browser_view), RuntimeStyle::DEFAULT);
    window_create_top_level(Some(&mut window_delegate));
}

wrap_browser_view_delegate! {
    struct WikiBrowserViewDelegate {
        runtime_style: RuntimeStyle,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            let shape = read_wiki_shape();
            Size {
                width: shape.width,
                height: shape.height,
            }
        }
    }

    impl BrowserViewDelegate {
        fn browser_runtime_style(&self) -> RuntimeStyle {
            self.runtime_style
        }
    }
}

wrap_window_delegate! {
    struct WikiWindowDelegate {
        browser_view: RefCell<Option<BrowserView>>,
        runtime_style: RuntimeStyle,
    }

    impl ViewDelegate {}

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let browser_view = self.browser_view.borrow();
            let (Some(window), Some(browser_view)) = (window, browser_view.as_ref()) else {
                return;
            };
            window.set_to_fill_layout();
            let mut view = View::from(browser_view);
            window.add_child_view(Some(&mut view));
            let title = CefString::from("Pokem Online · Wiki");
            window.set_title(Some(&title));
            if let Some(mut icon) = load_icon() {
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }
            window.set_accelerator(CMD_FULLSCREEN, KEY_F11, 0, 0, 0, 1);
            window.set_accelerator(CMD_RELOAD, KEY_F5, 0, 0, 0, 1);
            // The game is usually playing behind the wiki, so mute works here too.
            window.set_accelerator(CMD_MUTE, KEY_END, 0, 0, 0, 1);

            // Where he left it last time, or centred the first time. A maximised window
            // takes its own bounds, so leave those alone.
            let shape = read_wiki_shape();
            if !shape.maximized {
                if shape.placed {
                    window.set_bounds(Some(&Rect {
                        x: shape.x,
                        y: shape.y,
                        width: shape.width,
                        height: shape.height,
                    }));
                } else {
                    window.center_window(Some(&Size {
                        width: shape.width,
                        height: shape.height,
                    }));
                }
            }

            if let Some(browser) = browser_view.browser() {
                if let Ok(mut held) = WIKI_BROWSER.lock() {
                    *held = Some(browser);
                }
            }
            if let Ok(mut held) = WIKI_WINDOW.lock() {
                *held = Some(window.clone());
            }
            window.show();
            window.activate();
        }

        fn on_window_closing(&self, window: Option<&mut Window>) {
            if let Some(window) = window {
                keep_wiki_shape(window);
            }
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
            if let Ok(mut held) = WIKI_WINDOW.lock() {
                *held = None;
            }
            if let Ok(mut held) = WIKI_BROWSER.lock() {
                *held = None;
            }
        }

        fn on_accelerator(&self, window: Option<&mut Window>, command_id: i32) -> i32 {
            let Some(window) = window else { return 0 };
            match command_id {
                CMD_FULLSCREEN => {
                    let fullscreen = window.is_fullscreen() == 1;
                    window.set_fullscreen((!fullscreen) as i32);
                    1
                }
                CMD_RELOAD => {
                    if let Some(browser_view) = self.browser_view.borrow().as_ref() {
                        if let Some(browser) = browser_view.browser() {
                            browser.reload();
                        }
                    }
                    1
                }
                CMD_MUTE => {
                    toggle_mute();
                    1
                }
                _ => 0,
            }
        }

        fn can_resize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_maximize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn initial_show_state(&self, _window: Option<&mut Window>) -> ShowState {
            if read_wiki_shape().maximized {
                ShowState::MAXIMIZED
            } else {
                ShowState::NORMAL
            }
        }

        fn window_runtime_style(&self) -> RuntimeStyle {
            self.runtime_style
        }
    }
}

wrap_life_span_handler! {
    struct WikiLifeSpanHandler {}

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let Some(url) = target_url else { return 0 };
            open_external(&url.to_string());
            1
        }
        // No on_before_close: closing the wiki leaves the game running. The game's own
        // handler is what quits the message loop.
    }
}

wrap_request_handler! {
    struct WikiRequestHandler {}

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: i32,
            _is_redirect: i32,
        ) -> i32 {
            let (Some(frame), Some(request)) = (frame, request) else {
                return 0;
            };
            if frame.is_main() != 1 {
                return 0;
            }
            let url = CefString::from(&request.url()).to_string();
            // The wiki is the only thing this window shows. The game and the rest of
            // the web belong to the player's browser.
            if is_wiki_url(&url) {
                return 0;
            }
            if url.starts_with("https://") || url.starts_with("http://") {
                open_external(&url);
                return 1;
            }
            0
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
        ) -> i32 {
            let Some(url) = target_url else { return 0 };
            open_external(&url.to_string());
            1
        }
    }
}

wrap_display_handler! {
    struct WikiDisplayHandler {}

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            // The window names itself, so the taskbar can tell it from the game.
            let page = title.map(|text| text.to_string()).unwrap_or_default();
            let page = page.trim().to_owned();
            let label = if page.is_empty() { "Wiki".to_owned() } else { page };
            let full = CefString::from(format!("{label} · Pokem Online").as_str());
            if let Ok(held) = WIKI_WINDOW.lock() {
                if let Some(window) = held.as_ref() {
                    window.set_title(Some(&full));
                }
            }
        }
    }
}

wrap_client! {
    struct WikiClient {}

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(WikiLifeSpanHandler::new())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(WikiRequestHandler::new())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(WikiDisplayHandler::new())
        }
    }
}

wrap_client! {
    struct LauncherClient {}

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(LauncherLifeSpanHandler::new())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(LauncherRequestHandler::new())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(LauncherDisplayHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(LauncherLoadHandler::new())
        }
    }
}

/// Bring the game back after its renderer died. Two deaths inside a minute mean the
/// page is dying as it loads, so the second one lands on the offline page, whose own
/// timer keeps trying, instead of spinning on a broken page.
fn recover(browser: Option<&mut Browser>) {
    let Some(browser) = browser else { return };
    let Some(frame) = browser.main_frame() else {
        return;
    };

    let now = now_ms();
    let previous = LAST_RECOVERY_MS.swap(now, Ordering::Relaxed);
    let looping = now.saturating_sub(previous) < CRASH_GAP_MS;
    let target = if looping {
        offline_url()
    } else {
        GAME_URL.to_string()
    };
    let url = CefString::from(target.as_str());
    frame.load_url(Some(&url));
}

wrap_request_handler! {
    struct LauncherRequestHandler {}

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: i32,
            _is_redirect: i32,
        ) -> i32 {
            let (Some(frame), Some(request)) = (frame, request) else {
                return 0;
            };
            // Only the window's own page is ours to police; sub-frames are the game's.
            if frame.is_main() != 1 {
                return 0;
            }
            let url = CefString::from(&request.url()).to_string();
            if is_wiki_url(&url) {
                // The wiki gets its own window, in this process, instead of the player's
                // browser. Posted rather than built here: a top-level window is not
                // something to create inside a navigation callback.
                let mut task = ShowWiki::new(url);
                post_task(ThreadId::UI, Some(&mut task));
                return 1;
            }
            if is_game_url(&url) || is_launcher_page(&url) {
                return 0;
            }
            // Anything that is not a web page (about:blank while the view is created,
            // data:, blob:) is the page's own business.
            if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file://")) {
                return 0;
            }
            open_external(&url);
            1
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
        ) -> i32 {
            let Some(url) = target_url else { return 0 };
            let url = url.to_string();
            open_external(&url);
            1
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: i32,
            _error_string: Option<&CefString>,
        ) {
            // A renderer going away during shutdown is the window closing, not a crash.
            if CLOSING.load(Ordering::Relaxed) {
                return;
            }
            recover(browser);
        }
    }
}

wrap_display_handler! {
    struct LauncherDisplayHandler {}

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            let Some(message) = message else { return 0 };
            let text = message.to_string();
            let Some(payload) = text.strip_prefix("PO ") else {
                return 0;
            };
            // `POKEM_DEBUG=1` echoes what the page reports, for checking presence values
            // without a debugger attached.
            if debug_echo() {
                eprintln!("{text}");
            }
            if payload.starts_with('{') {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                    // The pill speaks the same `PO` channel as presence: a click is a
                    // small command, not a state report.
                    if let Some(cmd) = value.get("cmd").and_then(|field| field.as_str()) {
                        match cmd {
                            "update" => start_update(),
                            "restart" => restart_now(),
                            _ => {}
                        }
                        return 0;
                    }
                    let field = |name: &str| {
                        value
                            .get(name)
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    };
                    let snapshot = Snapshot {
                        name: field("name"),
                        place: field("place"),
                        region: field("region"),
                        activity: field("activity"),
                    };
                    // Kept for the wiki key, which wants the map rather than the floor.
                    if let Ok(mut held) = WHERE.lock() {
                        *held = field("map");
                    }
                    if let Some(tx) = PRESENCE_TX.get() {
                        let _ = tx.send(snapshot);
                    }
                }
            }
            0
        }
    }
}

wrap_load_handler! {
    struct LauncherLoadHandler {}

    impl LoadHandler {
        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            let Some(frame) = frame else { return };
            if frame.is_main() != 1 {
                return;
            }
            // A load always has a live browser behind it, unlike the window creation
            // callback, so this is the handle the notice task should use.
            if let Some(browser) = browser.as_ref().map(|held| (*held).clone()) {
                if let Ok(mut held) = BROWSER.lock() {
                    *held = Some(browser);
                }
            }
            let url = CefString::from(&frame.url()).to_string();
            // The check can land before the splash does, so this is where the pill is
            // asked to appear on a launch where the cache already had the answer.
            if is_splash_url(&url) {
                if let Some(version) = UPDATE_READY.lock().ok().and_then(|held| held.clone()) {
                    apply_update_notice(&version);
                }
                return;
            }
            if !url.starts_with("https://pokem.onl") {
                return;
            }
            let script_url = CefString::from(GAME_URL);
            let probe = CefString::from(PROBE_JS);
            frame.execute_java_script(Some(&probe), Some(&script_url), 0);
            let files_help = CefString::from(
                FILES_HELP_JS.replace("__FILES_HELP__", FILES_HELP_URL).as_str(),
            );
            frame.execute_java_script(Some(&files_help), Some(&script_url), 0);
            let wiki_links = CefString::from(WIKI_LINKS_JS);
            frame.execute_java_script(Some(&wiki_links), Some(&script_url), 0);

            // The check may land after the splash is gone, and a reload brings the page
            // back without the pill, so the game page asks for it again here.
            if let Some(version) = UPDATE_READY.lock().ok().and_then(|held| held.clone()) {
                apply_update_notice(&version);
            }

            // A console that runs in the page, for testing the site's own API with the
            // session the page already has. Devtools is a second window, so it exists only
            // when POKEM_DEVTOOLS=1 asks for it.
            if std::env::var("POKEM_DEVTOOLS").is_ok() && !DEVTOOLS_OPENED.swap(true, Ordering::SeqCst) {
                if let Some(host) = browser.as_ref().and_then(|b| b.host()) {
                    host.show_dev_tools(None, None, None, None);
                }
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let Some(frame) = frame else { return };
            if frame.is_main() != 1 {
                return;
            }
            // -3 ERR_ABORTED means a newer load replaced this one, which is not a failure.
            if error_code == Errorcode::ABORTED {
                return;
            }
            let failed = failed_url.map(|url| url.to_string()).unwrap_or_default();
            // The retry page asking for the game again is the retry page's business; it
            // already has its own timer and reloading it here would only reset that.
            if is_launcher_page(&failed) {
                return;
            }
            let url = CefString::from(offline_url().as_str());
            frame.load_url(Some(&url));
        }
    }
}

wrap_life_span_handler! {
    struct LauncherLifeSpanHandler {}

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let Some(url) = target_url else { return 0 };
            let url = url.to_string();
            // One window is the whole point, so no popup is ever allowed; a link the
            // page wanted in a new window goes to the player's browser instead.
            open_external(&url);
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            CLOSING.store(true, Ordering::Relaxed);
            // A checked update is renamed into place now. The running image keeps its
            // own inode, so this is the moment that costs nothing.
            install_on_quit();
            quit_message_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wiki_is_its_own_page_on_the_game_origin() {
        for url in [
            WIKI_URL,
            "https://pokem.onl/wiki/",
            "https://pokem.onl/wiki/places/kanto",
            "https://pokem.onl/wiki?q=bulbasaur",
        ] {
            assert!(is_wiki_url(url), "{url} should be the wiki");
            assert!(is_game_url(url), "{url} is still the game's origin");
        }
    }

    #[test]
    fn the_wiki_key_opens_the_map_you_are_standing_on() {
        assert_eq!(
            wiki_url_for(Some("Saffron City")),
            "https://pokem.onl/wiki#/place?q=Saffron%20City"
        );
        assert_eq!(
            wiki_url_for(Some("  Pallet Town  ")),
            "https://pokem.onl/wiki#/place?q=Pallet%20Town"
        );
        // The floor name is no longer a fallback: a building is not a place in the wiki's
        // map data, so with no map the key opens the wiki itself.
        assert_eq!(wiki_url_for(None), WIKI_URL);
        assert_eq!(wiki_url_for(Some("   ")), WIKI_URL);
    }

    #[test]
    fn the_user_agent_names_this_build() {
        let ua = user_agent();
        assert!(
            ua.contains(&format!("pokem-online/{}", env!("CARGO_PKG_VERSION"))),
            "{ua}"
        );
    }

    #[test]
    fn only_a_higher_version_counts_as_an_update() {
        // Relative to the version being built, so a bump does not need this test edited.
        let current = version_tuple(env!("CARGO_PKG_VERSION")).expect("the build carries a version");
        let (major, minor, patch) = current;
        let tag = |v: (u32, u32, u32)| format!("v{}.{}.{}", v.0, v.1, v.2);

        assert!(!is_newer(&tag(current)), "the running version is not an update");
        assert!(is_newer(&tag((major, minor + 1, patch))), "a higher minor is an update");
        let older = (major, minor.saturating_sub(1), 0);
        if older < current {
            assert!(!is_newer(&tag(older)), "an older version is not an update");
        }
        assert!(!is_newer("nightly"));
        assert_eq!(version_tuple("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(version_tuple("0.10.0"), Some((0, 10, 0)));
    }

    #[test]
    fn a_tag_cannot_carry_anything_but_a_version_into_the_notice() {
        assert_eq!(safe_version("v0.2.0"), "v0.2.0");
        assert_eq!(safe_version("v0.2.0\"><img onerror=x>"), "v0.2.0imgonerrorx");
    }

    #[test]
    fn the_pill_belongs_to_the_splash() {
        assert!(is_splash_url(&splash_url()));
        assert!(!is_splash_url(GAME_URL));
        assert!(!is_splash_url(&offline_url()));
    }

    #[test]
    fn a_version_cannot_break_out_of_the_call() {
        let js = splash_notice_js("v0.2.0\";alert(1);\"");
        assert!(js.contains("\"v0.2.0alert1\""), "{js}");
        assert!(!js.contains("alert(1)"), "{js}");
        assert!(js.contains(RELEASE_PAGE), "{js}");
    }

    #[test]
    fn the_wiki_ball_opens_the_species_route() {
        // The number the game puts on each portrait is the national number, which is what
        // the wiki's species route takes.
        assert!(WIKI_LINKS_JS.contains("https://pokem.onl/wiki#/pokemon/"));
        assert!(WIKI_LINKS_JS.contains("canvas[data-dex]"));
        assert!(WIKI_LINKS_JS.contains("a.pkwiki"));
        // The seen-only guard: a met species is `article.dexpage.dx`, an unseen one is a
        // plain section, so the ball cannot land on a species the player has not met.
        assert!(WIKI_LINKS_JS.contains("article.dexpage.dx"));
    }

    #[test]
    fn a_place_name_cannot_add_its_own_route() {
        assert_eq!(
            wiki_url_for(Some("A&B#/pokemon/1")),
            "https://pokem.onl/wiki#/place?q=A%26B%23%2Fpokemon%2F1"
        );
    }

    #[test]
    fn a_name_that_merely_starts_with_wiki_is_not_the_wiki() {
        for url in [
            "https://pokem.onl/wikileaks",
            "https://pokem.onl/wik",
            "https://pokem.onl/",
            "https://pokem.onl/wiki.example.test/",
            "https://evil.test/https://pokem.onl/wiki",
        ] {
            assert!(!is_wiki_url(url), "{url} is not the wiki");
        }
    }
}
