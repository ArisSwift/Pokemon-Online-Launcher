use cef::*;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::cell::RefCell;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_ID: &str = "1574411473172561931";
const APP_NAME: &str = "Pokem Online";
const GAME_URL: &str = "https://pokem.onl/";

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

fn splash_url() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/splash/index.html");
    format!("file://{}", path.display())
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
    }

    spawn_presence();

    let cache = cache_dir();
    clear_stale_singleton(&cache);

    let settings = Settings {
        no_sandbox: 1,
        cache_path: CefString::from(cache.as_str()),
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

            let mut window_delegate = LauncherWindowDelegate::new(
                RefCell::new(browser_view),
                RuntimeStyle::DEFAULT,
                ShowState::NORMAL,
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
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 800 }
        }
    }

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let browser_view = self.browser_view.borrow();
            let (Some(window), Some(browser_view)) = (window, browser_view.as_ref()) else {
                return;
            };
            let mut view = View::from(browser_view);
            window.add_child_view(Some(&mut view));
            let title = CefString::from(APP_NAME);
            window.set_title(Some(&title));
            if let Some(mut icon) = load_icon() {
                window.set_window_icon(Some(&mut icon));
                window.set_window_app_icon(Some(&mut icon));
            }
            if self.initial_show_state != ShowState::HIDDEN {
                window.show();
            }
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            *self.browser_view.borrow_mut() = None;
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

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(LauncherDisplayHandler::new())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(LauncherLoadHandler::new())
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
            _browser: Option<&mut Browser>,
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
            let code = CefString::from(PROBE_JS);
            let script_url = CefString::from(GAME_URL);
            frame.execute_java_script(Some(&code), Some(&script_url), 0);
        }
    }
}

wrap_life_span_handler! {
    struct LauncherLifeSpanHandler {}

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            quit_message_loop();
        }
    }
}
