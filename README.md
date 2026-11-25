# Pokem Online Launcher

A desktop window for [pokem.onl](https://pokem.onl), with Discord Rich Presence.

The game normally runs in a browser tab, so Discord has no idea what you are doing. This gives it a real window. It remembers its size and place, opens on a splash screen, and tells Discord which town you are standing in.

I built it for myself. It works, so it is here.

## What it does

- Opens pokem.onl in its own window, without browser chrome.
- Runs one copy at a time. Launching it again brings the open window forward instead of starting a second one.
- Remembers the window shape between launches.
- Shows your trainer, place and region on Discord while you play.
- Explains which game files the site wants when its first screen asks for them.
- Shows a local page when the site is down, instead of a Chromium error.

## What it does not do

It does not include any games. pokem.onl builds its world from copies you own, and the launcher only links to the page that explains how to make them. There is no tray icon, no second window and no login screen. You sign in on the site like you would in a browser.

## Download

Tagged versions are on the [releases page](https://github.com/ArisSwift/Pokemon-Online-Launcher/releases), and the builds are attached to each one. Grab the archive for your system, unpack it, and run `pokem-online`. On macOS, open `Pokem Online.app`.

I have only run this on Linux. The Windows and macOS builds compile in CI, but nobody has played them yet. If one breaks, open an issue and tell me what happened.

## Build it yourself

You need Rust, a C++ compiler, CMake and Ninja. CEF is around 100 MB and `cef-dll-sys` downloads it on the first build.

On Arch you can reuse the system `cef` package instead of downloading it:

```sh
ln -s /usr/lib/cef .cef
CEF_PATH=$PWD/.cef cargo build
```

If CMake is not on your PATH, point at your copy:

```sh
CEF_PATH=$PWD/.cef CMAKE=/path/to/cmake cargo build
```

Run it from the repo root:

```sh
./target/debug/pokem-online
```

A bare macOS binary cannot find the CEF framework, so build the app bundle:

```sh
cargo build --release
./target/release/bundle-macos release dist
open "dist/Pokem Online.app"
```

On Linux, the same release binary can go into an AppImage:

```sh
cargo build --release
APPIMAGETOOL=/path/to/appimagetool ./packaging/appimage/build.sh
```

That writes `dist/pokem-online-x86_64.AppImage`. It runs without Chromium's setuid sandbox, because an AppImage mount is nosuid. If `appimagetool` is already on your PATH you can leave the variable off. The release page carries one built by CI, so you only need this if you changed the code.

### Put it in your menu

An AppImage in a downloads folder is easy to lose. This copies it into `~/.local/bin`, lifts the icon out of the image, and writes a menu entry, all inside your home folder with no root:

```sh
./packaging/linux/install.sh ~/Downloads/pokem-online-x86_64.AppImage
```

With no argument it uses `dist/pokem-online-x86_64.AppImage`. It prints the three paths it wrote, and the one `rm` line that undoes them.

Every release carries the same script as `install.sh` next to the AppImage, so it can be downloaded without cloning the repository.

## How it fits together

The window is CEF Views, no UI toolkit. A local splash loads first, then swaps to `https://pokem.onl/` in the same window, so the session survives the change.

Discord presence comes from the page itself. The launcher reads the game's HUD and its own presence socket, so the card mirrors what the game knows instead of guessing from the URL.

## License

MIT. Take it, fork it, put it in a package, as long as the copyright line comes along.

That covers the Rust code. The Pokemon fonts, the splash sound and the artwork in `assets/` and `Icon/` are not mine to license. They are there because the first screen needs them, and they stay with their owners.

## Credits

pokem.onl is made by [Mitchell Winn](https://mitchart.net). This is a third-party launcher, not part of the game.

The Discord invite for the game is [here](https://discord.gg/DAkeDwkvSA) if you want to talk to the people who actually build it.
