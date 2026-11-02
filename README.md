<p align="center">
  <picture>
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/product/newskite-lockup-on-light.svg">
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/product/newskite-lockup-on-dark.svg">
    <img alt="newskite" src="assets/brand/product/newskite-lockup-on-dark.svg" width="420">
  </picture>
</p>

# Newskite

Newskite is a desktop Usenet (NZB) download client written in Rust. You give it an NZB file;
it downloads the listed articles from your news server (NNTP, usually over TLS), decodes the
yEnc payloads, writes the original files to disk byte for byte, then verifies, repairs and
extracts them without any external program. It keeps a resumable, reorderable queue
where every job can be started, paused or moved on its own, a download history, per-file
progress, a filterable log and a live throughput graph, and it follows the desktop's light
or dark color scheme (KDE Plasma first).

Drop `.nzb` files (or a folder of them) onto the window, or copy them in the file manager
and press Ctrl+V. On Wayland the window runs through XWayland, because the windowing
library cannot receive dropped files natively there; set `NEWSKITE_WAYLAND=1` to keep
native Wayland (Ctrl+V and Add NZB still work).

The same queue engine also runs without a window: `newskite --headless` downloads NZBs from
a terminal or a script.

Newskite does not come with a Usenet server subscription, and it does not search or browse
Usenet: NZB files are the input (open them, drop them on the window, or use a watch folder).
You need an account with a news provider.

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
         │ segment fetch, │  │ SQLite: jobs,  │  │ native par2, rar, │
         │ decode, writes │  │ settings, ...  │  │ 7z, zip, cleanup  │
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
| `newskite-par2` | Native PAR2 verify and repair (Reed-Solomon over GF(2^16)) |
| `newskite-postproc` | Post-processing: PAR2, native archive extraction (RAR, 7z, ZIP including spanned sets), cleanup |
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

## Where data is stored

Everything lives in one SQLite database. On Linux that is
`~/.local/share/newskite/db/newskite.sqlite3` (the `db` folder under the platform data
directory; on Windows and macOS it is under the usual per-user application data folder).
The headless `--db DIR` option points it elsewhere. Only one Newskite process can use a
data directory at a time: while the GUI is running, a headless run on the same directory
exits with an "another Newskite instance" error instead of touching the GUI's queue.

## Credential storage

Newskite does not use the system keyring and does not encrypt anything it stores. On Linux
and macOS the database files are created readable only by you (mode `0600`, and a data
directory Newskite creates is `0700`), but anyone with access to your account, root, or a
backup of the files, or anyone who can watch your processes, can recover these:

- **Server passwords** are stored unencrypted: the settings, including every server's
  username and password, are kept as plain JSON in the `settings` table of the database in
  `~/.local/share/newskite/db`.
- **Archive passwords** you enter for a job are stored in plain text with that job (the `jobs`
  table). Extraction is built in, so the password never leaves the Newskite process: it is
  kept in memory while an archive is extracted and never handed to another program or put on
  a command line.
- **Headless CLI:** prefer the `NEWSKITE_PASS` environment variable over `--pass`, because a
  password given on the command line is visible to other users in the process list. Passwords given with `--server`/`--pass`/`NEWSKITE_PASS` are kept only in
  memory and never written to the database.

Passwords are never written to the message log.

## Post-processing

PAR2 verify/repair and RAR, 7z and ZIP extraction (including split volumes, spanned
`.z01` ... `.zip` sets and passwords) are built into Newskite. It never starts an external
program, so `par2`, `7z`, `unrar` and `unzip` are not needed and not used even when they are
installed. A download with missing articles is still repaired when its PAR2 data covers the
gap. An archive that uses a feature the built-in extraction does not support (a rare codec)
fails with a message naming it; a PAR2 set whose main packet cannot be read is reported as
unverified, and extraction still runs and checks the archive's own checksums.

Building from source compiles the bundled UnRAR library, so a C++ compiler (`g++` or
`clang++`) is needed.

## Packaging

All packaging files are in [`packaging/`](packaging):

| File | Purpose |
|---|---|
| `net.innerpulse.Newskite.desktop` | Desktop entry; registers `application/x-nzb`, `nzb:` and `nzblnk:` |
| `net.innerpulse.Newskite.metainfo.xml` | AppStream metadata for software centers |
| `mime/newskite-nzb.xml` | shared-mime-info type `application/x-nzb` (`*.nzb` glob and `<nzb` magic) |
| `icons/net.innerpulse.Newskite.svg` | Scalable app icon |
| `icons/hicolor/<size>/apps/*.png` | PNG renders at 16, 24, 32, 48, 64, 128, 256 and 512 px |
| `icons/light/` | The same set on a light (paper) tile, for light docks and launchers |
| `icons/make-icons.mjs` | Generates every icon file above (see [Brand](#brand)) |
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
and icons under `/usr/share` and depends on the windowing libraries listed above; it needs no
post-processing tools. The glibc dependency comes from the build machine, so build on the
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

### Regenerating the icons

The icons are generated, not hand-drawn; see [Brand](#brand) for the command.

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

The binary also includes the UnRAR library by Alexander Roshal, which reads RAR archives
under its own freeware license: it may be redistributed freely, but its code may not be used
to create RAR archives or to rebuild the RAR compression algorithm. The full text is in
[`packaging/debian/copyright`](packaging/debian/copyright); the `.deb` installs it as
`/usr/share/doc/newskite/copyright` and the AppImage carries it at the same path inside.

## Brand

Newskite is an [innerpulse](https://innerpulse.net) product.

The app icon is the stock innerpulse app icon at every size: the ink tile and the six-bar
mark. Following the brand's rule for desktop apps, the product's violet accent appears inside
the app, not in the icon. The kit's whole-pixel renders keep the small sizes sharp.

- `assets/brand/product/`: the Newskite wordmark and horizontal lockups (on dark and on light)
  and the social card.
- `assets/brand/kit/`: the stock innerpulse mark, app icons and UI icons, copied from the
  brand kit.
- `assets/brand/fonts/`: the brand typefaces (Instrument Sans, JetBrains Mono).
- `assets/brand/fonts/OFL.txt`: the fonts' copyright notices and license (SIL OFL 1.1). The
  fonts are embedded in the binary, so the packages install this file as
  `/usr/share/doc/newskite/fonts-OFL.txt`, with a machine-readable `packaging/debian/copyright`.
- `assets/brand/app-icon-256.png`: the 256 px app icon, a copy of the hicolor render.
- `packaging/icons/`: the installed app icons and `preview.png`, a contact sheet of every size.

To reinstall the icons from the kit copy (set `BRAND_KIT` to the innerpulse-brand kit to
refresh that copy first; ImageMagick, when installed, redraws `preview.png`):

```sh
BRAND_KIT=/path/to/innerpulse-brand node packaging/icons/make-icons.mjs
packaging/tests/check-packaging.sh
```
