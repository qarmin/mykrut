# mykrut

A fast, modern file manager for Linux, built with Rust and Slint.

The design was heavily inspired by [Nemo](https://github.com/linuxmint/nemo), which covered almost all my needs perfectly. The main pain points that led me to write my own: occasional crashes, icon flickering on mixed-DPI setups with multiple monitors, and very slow thumbnail rendering when browsing directories with thousands of images.

> **Linux only.** The app uses zbus (UDisks2), MTP and inotify - no Windows/macOS support is planned.

## Features

- Two-pane split view - independent tabs, history and listing per pane, resizable divider
- Tabs with per-tab history (back/forward/up)
- Grid and list views with resizable/toggleable columns, independent icon/row size scrubber
- Places sidebar - Computer/Places/Devices/Bookmarks, with per-device disk-usage bar
- Device support - UDisks2 mounting via zbus, MTP device detection
- File operations - copy, move, delete/trash, rename, bulk rename with template patterns
- Archives - compress and extract common formats
- Search, properties (with deep directory size counting), bulk-open confirmation
- Bookmarks, monochrome/colored icon sets, dark mode
- **No automatic folder watching by default** - this is intentional: it keeps the app lean and fast even in directories with thousands of files. An opt-in filesystem watcher exists as an experimental setting.

## Build & run

```bash
cargo build --release
# binary: target/release/mykrut
```

Or with `just`:

```bash
just run     # debug
just runr    # release
```

### Release binaries (Linux, portable)

Requires [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) and a Rust target:

```bash
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
just binaries   # output: binaries/linux_mykrut
```

## License

GPL-3.0-only - see [LICENSE](LICENSE)

---

> **Note:** this project was developed with AI assistance (Claude Code).
