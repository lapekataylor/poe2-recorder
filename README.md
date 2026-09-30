# PoE Recorder

PoE Recorder records Path of Exile 2 on Linux. It follows the game's
`Client.txt` log, starts recording when you enter a waystone map, stops when
you leave it, and keeps a library of the videos with a metadata file beside
each one. It is a native Rust and GTK4 application, built as a Flatpak for
Wayland sessions.

It is a modified version of [Warcraft Recorder](https://github.com/aza547/wow-recorder),
based on JohanWes's native Linux port,
[wow-recorder-linuxwayland](https://github.com/JohanWes/wow-recorder-linuxwayland).
Path of Exile 1 support is planned.

## Features

- **Automatic map recording.** Entering a map starts a recording.
  `gpu-screen-recorder` keeps a replay buffer, so each video starts a few
  seconds before the map loaded.
- **Portal trips stay in one video.** Leaving the map starts a grace period
  (5 minutes by default). Coming back to the same map within it continues
  the recording; entering a different map ends the previous run at once.
- **Sub-areas count as the map.** Abyss depths and similar areas entered from
  inside a map are part of the run.
- **No hideout tail.** When a run is saved, the video is cut five seconds
  after you last left the map.
- **Picks up where you are.** Starting the app while you are in a map begins
  recording straight away.
- **Timeline markers** for deaths and for time spent out of the map.
- **Library** with a Map runs section showing map, area level, deaths,
  duration and date. Recordings can be tagged, protected and deleted, and the
  oldest unprotected recordings are removed above the storage limit.
- **Player** with playback speeds from 0.25x to 2x, frame stepping while
  paused (`,` and `.`), jumping between markers (`[` and `]`), and clipping.
- **Background recording** from a tray icon.

The code still contains the World of Warcraft support it was forked from; it
will be removed.

## Requirements

- A Wayland session. X11 is not supported.
- Flatpak and `flatpak-builder`, with Flathub for the GNOME 50 SDK.
- `xdg-desktop-portal` with a ScreenCast backend for your desktop, and
  PipeWire.
- A GPU with hardware video encoding.

`gpu-screen-recorder`, the Clapper video player and FFmpeg are bundled in the
Flatpak.

## Build and install

There are no prebuilt releases yet. Build the development Flatpak from the
repository root:

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean .flatpak-builder/build-devel \
  flatpak/io.github.lapekataylor.PoeRecorder.Devel.yml
```

The first build compiles FFmpeg, `gpu-screen-recorder` and Clapper and takes a
while; later builds reuse them. Start the app from your launcher as
**PoE Recorder (Development)**, or with
`flatpak run io.github.lapekataylor.PoeRecorder.Devel`.

## Setup

1. Open Settings, turn on **Path of Exile 2 logs** and choose the game's
   `logs` folder, for example
   `.../steamapps/common/Path of Exile 2/logs`.
2. Choose a **recording folder**.
3. When asked, choose the screen to capture.
4. Leave the app running and play. Each map run appears under **Map runs**
   when it ends.

The tray icon uses the StatusNotifierItem protocol. GNOME needs the
[AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/)
to show it. Without a tray, closing the window quits the app.

## Development

The app is one Cargo package under `native/`; the `Client.txt` parser and
map-run tracker are a separate, dependency-free crate in `native/poe2-log/`.
Building the app outside Flatpak needs the Clapper libraries
(`libclapper`, `libclapper-gtk`). From the repository root:

```sh
cargo fmt --manifest-path native/Cargo.toml --check
cargo clippy --manifest-path native/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path native/Cargo.toml --all-targets
cargo test --manifest-path native/poe2-log/Cargo.toml
```

`scripts/fake-poe2-log.py` writes simulated `Client.txt` lines for testing
without the game, and the `watch` example prints what the recorder would do
for a log:

```sh
cargo run --manifest-path native/poe2-log/Cargo.toml --example watch -- <Client.txt> --verbose
```

See [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md).

## License

GPL-3.0-or-later. Capture uses
[`gpu-screen-recorder`](https://git.dec05eba.com/gpu-screen-recorder/).
The app icon's lettering is [Cinzel](https://github.com/NDISCOVER/Cinzel-Typeface)
by Natanael Gama (SIL Open Font License 1.1), converted to outlines.
PoE Recorder is not affiliated with or endorsed by Grinding Gear Games.
