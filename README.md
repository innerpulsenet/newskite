# Newskite

Newskite is a desktop Usenet (NZB) download client written in Rust. You give it an NZB file;
it downloads the listed articles from your news server (NNTP, usually over TLS), decodes the
yEnc payloads, writes the original files to disk byte for byte, then verifies, repairs and
extracts them when the right tools are installed. It keeps a resumable queue, bookmarks,
a message log and a live throughput graph, and it follows the desktop's light or dark color
scheme (KDE Plasma first).

The same queue engine also runs without a window: `newskite --headless` downloads NZBs from
a terminal or a script.

Newskite does not come with a Usenet server subscription or a search indexer. You need an
account with a news provider.

The product specification is in [APP-OUTLINE.md](APP-OUTLINE.md) and the public API of every
crate is in [docs/CONTRACTS.md](docs/CONTRACTS.md).

## Architecture

The workspace is split into small crates with one job each. Arrows point from a crate to the
crates it depends on.

```
                        ┌──────────────────────────┐
                        │ newskite  (app binary)   │  egui/eframe window, launch args
                        └──┬─────────┬──────────┬──┘
                           │         │          │
               ┌───────────▼──┐  ┌───▼──────┐  ┌▼────────────────┐
               │ newskite-cli │  │ newskite │  │ newskite-theme  │  desktop color tokens
               │  (headless)  │  │ -service │◄─┤ (KDE/portal)    │
               └───────┬──────┘  └─┬──┬───┬─┘  └─────────────────┘
                       └──────────►│  │   │
                 ┌─────────────────┘  │   └────────────────┐
         ┌───────▼────────┐  ┌────────▼───────┐  ┌──────────▼────────┐
         │ newskite-engine│  │ newskite-store │  │ newskite-postproc │
         │ segment fetch, │  │ SQLite: jobs,  │  │ par2 / 7z / unrar │
         │ decode, writes │  │ settings, ...  │  │ / unzip, cleanup  │
         └──┬──────────┬──┘  └────────┬───────┘  └──────────┬────────┘
     ┌──────▼─────┐ ┌──▼───────────┐  │                     │
     │newskite-nzb│ │ newskite-nntp│  │                     │
     │ NZB + yEnc │ │ NNTP + TLS   │  │                     │
     └──────┬─────┘ └──┬───────────┘  │                     │
            └──────────┴──────────────┴──────────┬──────────┘
                                        ┌────────▼───────┐
                                        │ newskite-types │  settings, events, formatting
                                        └────────────────┘
```

| Crate | Role |
|---|---|
| `newskite-types` | Shared settings, server config, engine events, formatting helpers, default paths |
| `newskite-nzb` | NZB parser and yEnc decoder (byte-exact for all 256 byte values) |
| `newskite-nntp` | Async NNTP client (plain and TLS via rustls), plus a mock server used by tests |
| `newskite-engine` | Downloads one job: concurrent segment fetches, decoding, positioned writes, speed limit |
| `newskite-store` | SQLite persistence (bundled SQLite): jobs, files, bookmarks, throughput, settings |
| `newskite-postproc` | PAR2 verify/repair, archive extraction and cleanup through external tools |
| `newskite-service` | The queue shared by the GUI and the CLI: runs jobs, persists progress, watch folder, scheduler, auto-shutdown |
| `newskite-theme` | Reads the desktop color scheme (KDE `kdeglobals`, XDG portal, fallback) into color tokens |
| `newskite-cli` | Headless mode (`newskite --headless ...`) |
| `newskite` (`crates/newskite-app`) | The desktop application: egui/eframe GUI and launch-argument handling |

Stack: egui/eframe 0.33 (glow renderer), tokio, rustls (ring provider), rusqlite with bundled
SQLite.

## Building

Requirements:

- Rust 1.89 or newer (`rustup` recommended). The workspace MSRV is 1.89.
- A C compiler, for the bundled SQLite and the `ring` TLS crate.

On Debian, Ubuntu and Kubuntu that is all you need to build:

```sh
sudo apt install build-essential
cargo build --release -p newskite
```

No `-dev` packages are needed: the windowing libraries (Wayland, X11, xkbcommon, EGL/GL) are
loaded at run time, not linked at build time. To **run** the GUI, the system needs the usual
desktop libraries, which every desktop install already has: `libwayland-client0`,
`libwayland-cursor0`, `libwayland-egl1`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libx11-6`,
`libxcursor1`, `libxrandr2`, `libxi6`, `libegl1` and `libgl1`. The `.deb` declares these as
dependencies.

Run the tests with `cargo test --workspace`. The packaging files have their own check script,
`packaging/tests/check-packaging.sh` (see [Packaging](#packaging)).

## Running

### Desktop app

```sh
cargo run --release -p newskite                  # open the window
newskite ~/Downloads/some-release.nzb            # open the window and queue an NZB
newskite file:///home/me/Downloads/a.nzb         # file:// and nzb:///path URIs work too
```

Open Settings (gear icon or `Ctrl+,`) and add your news server: host, port (563 for TLS,
119 without), username, password and connection count. **Test** shows the server's actual
reply. Then import an NZB (toolbar, `Ctrl+O`, or drag it onto the window) and press Start.

Web links (`http(s)://`, `nzb://http...`, `nzblnk:`) are recognized but not fetched yet; the
app logs them as unsupported.

### Headless CLI

```sh
# Download with an explicit server (not saved to the settings):
NEWSKITE_PASS='secret' newskite --headless \
    --nzb ~/Downloads/a.nzb --dest ~/Downloads/Newskite \
    --server news.example.com:563 --tls --user alice --connections 20

# Several NZBs, using the server saved in the app's settings:
newskite --headless --nzb one.nzb --nzb two.nzb

# Use a separate database folder (for scripts and testing):
newskite --headless --db /tmp/nk-db --nzb a.nzb --server [::1]:119
```

It prints one `queued <name>` line per NZB, then `complete <name>` or `failed <name>: <reason>`
for each job. Exit codes: `0` when every job of the run completed, `1` when any failed, `2`
for usage errors (including "no usable server"), `130` after Ctrl-C (unfinished jobs stay
queued and resume on the next run). Prefer `NEWSKITE_PASS` over `--pass`, because command-line
arguments are visible to other users in the process list. `newskite --headless --help` lists
every option.

## Where data and credentials are stored

Everything lives in one SQLite database. On Linux that is
`~/.local/share/newskite/db/newskite.sqlite3` (the `db` folder under the platform data
directory; on Windows and macOS it is under the usual per-user application data folder).
The headless `--db DIR` option points it elsewhere.

**Server passwords are stored unencrypted.** The settings, including every server's
username and password, are kept as plain JSON in the `settings` table of that database.
Archive passwords you enter for a job are stored in plain text in the `jobs` table. Anyone
who can read your home directory can read them. Newskite does not use the system keyring.
Passwords given to the headless CLI with `--server`/`--pass`/`NEWSKITE_PASS` are kept only in
memory and never written to the database. Passwords are not written to the log.

## Post-processing tools

Repair and extraction use external programs, found on `PATH` at run time. Each one is
optional: when it is missing, that step is skipped and the job log says why.

| Tool | Used for | Debian/Ubuntu package |
|---|---|---|
| `par2` | PAR2 verify and repair | `par2` |
| `7z` (or `7zz`, `7za`) | 7z, ZIP and most RAR archives | `7zip` (or `p7zip-full` on older releases) |
| `unrar` | RAR archives (preferred for RAR) | `unrar` (non-free) |
| `unzip` | ZIP archives when 7-Zip is missing | `unzip` |

```sh
sudo apt install par2 7zip unzip     # plus unrar from the non-free/multiverse component
```

## Packaging

All packaging files are in [`packaging/`](packaging):

| File | Purpose |
|---|---|
| `net.innerpulse.Newskite.desktop` | Desktop entry; registers `application/x-nzb`, `nzb:` and `nzblnk:` |
| `net.innerpulse.Newskite.metainfo.xml` | AppStream metadata for software centers |
| `mime/newskite-nzb.xml` | shared-mime-info type `application/x-nzb` (`*.nzb` glob and `<nzb` magic) |
| `icons/net.innerpulse.Newskite.svg` | Scalable app icon |
| `icons/net.innerpulse.Newskite-16.svg` | Simplified source for the 16 px render |
| `icons/hicolor/<size>/apps/*.png` | PNG renders at 16, 32, 48, 64, 128 and 256 px |
| `build-appimage.sh` | Builds an AppDir and, with `appimagetool`, an `.AppImage` |
| `install-local.sh` / `uninstall-local.sh` | Per-user install into `~/.local` without root |
| `tests/check-packaging.sh` | Validates all of the above in a temporary directory |

The application id is `net.innerpulse.Newskite` everywhere: desktop file name, icon name,
AppStream id and the Wayland `app_id` / X11 `WM_CLASS` the window sets.

### Debian package (.deb)

```sh
cargo install cargo-deb --locked     # once
cargo deb -p newskite
# -> target/debian/newskite_0.1.0-1_amd64.deb
sudo apt install ./target/debian/newskite_0.1.0-1_amd64.deb
```

The metadata is in `[package.metadata.deb]` in `crates/newskite-app/Cargo.toml`. The package
installs the binary to `/usr/bin/newskite` plus the desktop entry, AppStream file, MIME type
and icons under `/usr/share`, depends on the windowing libraries listed above, and recommends
`par2`, `7zip` and `unzip`. The glibc dependency comes from the build machine, so build on the
oldest release you want to support.

### AppImage

```sh
packaging/build-appimage.sh                  # builds the binary, AppDir and AppImage
packaging/build-appimage.sh --appdir-only    # stop after the AppDir
```

The script never downloads anything. If `appimagetool` is not installed it leaves the AppDir
in `target/appimage/Newskite.AppDir`, prints where to get the tool, and exits with status 3.
Put `appimagetool` on `PATH` (or set `APPIMAGETOOL=/path/to/appimagetool`) and run
`packaging/build-appimage.sh --no-build`.

### Per-user install

```sh
packaging/install-local.sh          # binary to ~/.local/bin, desktop/MIME/icons to ~/.local/share
packaging/uninstall-local.sh        # removes exactly those files; keeps your data and downloads
```

`install-local.sh` refreshes the desktop, MIME and icon caches and makes Newskite the default
handler for `.nzb` files and `nzb:`/`nzblnk:` links (`--no-default` skips that). See `--help`
on each script for the options.

### Regenerating the PNG icons

The PNGs are rendered from the SVGs with `rsvg-convert` (package `librsvg2-bin`):

```sh
cd packaging/icons
for s in 32 48 64 128 256; do
  rsvg-convert -w $s -h $s net.innerpulse.Newskite.svg -o hicolor/${s}x${s}/apps/net.innerpulse.Newskite.png
done
rsvg-convert -w 16 -h 16 net.innerpulse.Newskite-16.svg -o hicolor/16x16/apps/net.innerpulse.Newskite.png
```

Inkscape (`inkscape -w S -h S -o out.png in.svg`) or ImageMagick (`magick -background none
-density 384 in.svg -resize SxS out.png`) work too.

## Platform status

- **Linux:** the primary target, developed on Kubuntu (KDE Plasma). The test suite covers the
  engine end to end against a local NNTP server and renders the GUI offscreen; the packaged
  window still needs hands-on testing on Wayland and X11 sessions. Desktop theme detection
  reads KDE's `kdeglobals` and the XDG desktop portal.
- **Windows and macOS:** the code is written to compile there (platform-specific code is
  behind `cfg`, paths come from the `directories` crate), but neither target has been built
  or tested yet, and there is no installer for them. The theme falls back to the toolkit's
  light/dark detection on those systems.

## License

Licensed under either of the MIT license or the Apache License, Version 2.0, at your option
(`MIT OR Apache-2.0`, as declared in `Cargo.toml`).
