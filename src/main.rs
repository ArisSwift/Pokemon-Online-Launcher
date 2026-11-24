use cef::*;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_ID: &str = "1574411473172561931";
const APP_NAME: &str = "Pokem Online";
const GAME_URL: &str = "https://pokem.onl/";

/// Chromium's own Linux agent with two tokens added. `pokem-online` says who we are, and
/// `Electron/` is the token the site looks for to tell its desktop app from a browser tab:
/// without it, the character screen carries an offer to download the official launcher
/// (`bCe()` in the game bundle returns a platform for a plain browser agent and null for
/// anything matching `/Electron\//i`).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) pokem-online/0.1.0 Chrome/156.0.0.0 Electron/156.0.0 Safari/537.36";

/// The renderer dying twice inside a minute means the page is dying as it loads, so the
/// recovery goes by way of the offline page instead of spinning on a broken page.
const CRASH_GAP_MS: u64 = 60_000;

/// Accelerator ids for the keys a bare Chromium window does not wire up itself.
const CMD_FULLSCREEN: i32 = 1;
const CMD_RELOAD: i32 = 2;
/// F11 and F5 from Chromium's keyboard codes.
const KEY_F11: i32 = 0x7a;
const KEY_F5: i32 = 0x74;

static LAST_RECOVERY_MS: AtomicU64 = AtomicU64::new(0);
/// Set while the browser is closing, so a renderer going away is not mistaken for a
/// crash and reloaded on the way out.
static CLOSING: AtomicBool = AtomicBool::new(false);
/// Devtools opens once, not on every navigation.
static DEVTOOLS_OPENED: AtomicBool = AtomicBool::new(false);

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

/// The first run, in our own words. The game already has a proper files gate, so this
/// does not explain anything twice: it only makes the first visit obvious, and its
/// button clicks the game's own "Add game files" so the picker still opens from a real
/// gesture. It removes itself once files are added.
const FIRSTRUN_JS: &str = r##"
(function () {
  if (window.__pokemFirstRun) return;
  window.__pokemFirstRun = true;

  var ID = "pokem-firstrun";

  function ensure() {
    var missing = document.querySelector(".missing-files");
    var existing = document.getElementById(ID);
    if (!missing) {
      if (existing) existing.remove();
      return;
    }
    if (existing) return;

    var bar = document.createElement("div");
    bar.id = ID;
    bar.setAttribute("style", [
      "all: initial",
      "position: fixed",
      "left: 50%",
      "bottom: 18px",
      "transform: translateX(-50%)",
      "z-index: 2147483647",
      "display: flex",
      "align-items: center",
      "gap: 14px",
      "padding: 12px 14px 12px 16px",
      "border-radius: 12px",
      "background: #1b1b1f",
      "border-left: 4px solid #ee3b2b",
      "box-shadow: 0 10px 30px rgba(0,0,0,.45)",
      "max-width: 560px"
    ].join(";"));

    var text = document.createElement("span");
    text.setAttribute("style", "all: initial;font: 14px/1.4 system-ui, sans-serif;color: #f2f2f4;");
    text.textContent = "This game is built from a copy you own. Your files stay on this device.";

    var button = document.createElement("button");
    button.setAttribute("style", [
      "all: initial",
      "cursor: pointer",
      "padding: 9px 14px",
      "border-radius: 8px",
      "background: #ee3b2b",
      "color: #fff",
      "font: 600 14px/1 system-ui, sans-serif",
      "white-space: nowrap"
    ].join(";"));
    button.textContent = "Add game files";
    button.addEventListener("click", function (event) {
      event.preventDefault();
      event.stopPropagation();
      var target = document.querySelector("[data-files]");
      if (target) target.click();
    });

    bar.appendChild(text);
    bar.appendChild(button);
    document.body.appendChild(bar);
  }

  ensure();
  setInterval(ensure, 2000);
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

fn main() {
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

    spawn_presence();

    let cache = cache_dir();
    clear_stale_singleton(&cache);

    // POKEM_UA is only for testing what the site does with a different agent.
    let user_agent = std::env::var("POKEM_UA").unwrap_or_else(|_| USER_AGENT.to_string());
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

    run_message_loop();
    shutdown();
}

fn cache_dir() -> String {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    base.join("pokem-online").to_string_lossy().into_owned()
}

/// Chromium leaves a SingletonLock symlink (`<host>-<pid>`) behind when the browser
/// process dies without a clean shutdown, and on this build it keeps refusing the
/// profile afterwards instead of noticing the pid is gone. Clear the lock ourselves
/// when that pid is dead; leave it alone when it is alive, so a real second launch
/// still gets Chromium's own "profile in use" handling.
#[cfg(target_os = "linux")]
fn clear_stale_singleton(cache: &str) {
    let root = std::path::Path::new(cache);
    let Ok(target) = std::fs::read_link(root.join("SingletonLock")) else {
        return;
    };
    let Some(pid) = target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return;
    };
    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
        return;
    }
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
                .buttons(vec![activity::Button::new("Play", GAME_URL)]);

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
            if self.initial_show_state != ShowState::HIDDEN {
                window.show();
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
            let url = CefString::from(&frame.url()).to_string();
            if !url.starts_with("https://pokem.onl") {
                return;
            }
            let script_url = CefString::from(GAME_URL);
            let probe = CefString::from(PROBE_JS);
            frame.execute_java_script(Some(&probe), Some(&script_url), 0);
            let first_run = CefString::from(FIRSTRUN_JS);
            frame.execute_java_script(Some(&first_run), Some(&script_url), 0);

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
            quit_message_loop();
        }
    }
}
