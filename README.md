# Lune

A fast, modern Wayland terminal emulator for Linux.

## Features

- **Smooth GPU Rendering**: Sharp text with ligatures and font fallback, inline images via the Kitty graphics protocol, optional background images, and smooth animated cursor.
- **Classic & Modern Modes**: Switch anytime between a minimal classic window and a modern layout with an inline titlebar or collapsible left tab sidebar.
- **Tabs & Split Panes**: Split horizontally or vertically, drag or resize dividers, zoom a pane, swap panes, detach a pane into a new tab, broadcast input to all panes in a tab, and rename tabs inline.
- **Command Blocks & Bookmarks**: Automatic shell integration (`bash`, `zsh`, `fish`) with clickable command blocks, sticky command headers while scrolling long output, bookmarks, quick jump to previous/failed commands, one-click copy command/output, and re-run.
- **Search, Vi Mode & Quick Select**: Literal and regex scrollback search (full history or inside a single command block), Vi navigation and visual selection, Quick Select hints for URLs and paths, fuzzy Command Palette with live theme preview, and right-click context menus.
- **Clipboard & Integration**: Standard clipboard and middle-click paste, drag-and-drop file paths, desktop notifications, and live configuration reloading.

## Building

Requires Linux (Wayland) and a Rust toolchain:

```bash
cargo build --release --bin lune
./target/release/lune
```

## Packaging

With [`just`](https://github.com/casey/just) installed, build Linux packages into `dist/`:

```bash
just build-linux-rpm        # .rpm
just build-linux-deb        # .deb
just build-linux-arch       # .pkg.tar.zst
just build-linux-tar        # .tar.gz
just build-linux-appimage   # .AppImage
just build-linux-all        # all formats
```

## Usage

```text
Usage: lune [OPTIONS] [-- COMMAND [ARGS...]]

Options:
  -d, --working-directory <DIR>  Start the initial session in DIR
  -T, --title <TITLE>            Set the initial window title
      --class, --app-id <ID>     Set the Wayland application ID (default: lune)
  -c, --config <PATH>            Load configuration from PATH
  -e, --command <CMD> [ARGS...]  Run CMD instead of the default shell
  -h, --help                     Print this help message and exit
  -V, --version                  Print version information and exit
```

## Configuration

Lune reads `${XDG_CONFIG_HOME:-~/.config}/lune/lune.conf` and reloads automatically on save. See [docs/lune.conf.example](docs/lune.conf.example) for all available options and keybind overrides.

## Default Shortcuts

| Action | Shortcut |
| --- | --- |
| Copy / Paste | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| Command Palette | `Ctrl+Shift+P` |
| Search / Quick Select / Vi Mode | `Ctrl+Shift+F` / `Ctrl+Shift+S` / `Ctrl+Shift+Space` |
| New Window / New Tab / Close Pane or Tab | `Ctrl+Shift+N` / `Ctrl+Shift+T` / `Ctrl+Shift+W` |
| Switch Tabs | `Ctrl+Tab`, `Ctrl+Shift+Tab`, `Ctrl+Shift+Left/Right`, `Alt+1..9` |
| Move Tab Left / Right | `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown` |
| Rename Tab | `Ctrl+Alt+R` |
| Split Right / Split Down | `Ctrl+Shift+D` / `Ctrl+Shift+E` |
| Focus / Resize Split Pane | `Ctrl+Alt+Arrows` (`H/J/K/L`) / `Ctrl+Alt+Shift+Arrows` (`H/J/K/L`) |
| Zoom Pane / Swap Pane / Detach Pane | `Ctrl+Shift+Z` / `Ctrl+Alt+S` / `Ctrl+Alt+T` |
| Broadcast Input | `Ctrl+Alt+I` |
| Switch Mode / Toggle Sidebar / Fullscreen | `Ctrl+Shift+M` / `Ctrl+Alt+B` / `F11` |
| Previous / Next Prompt / Failed Command | `Ctrl+Shift+Up` / `Ctrl+Shift+Down` / `Ctrl+Shift+X` |
| Copy Command / Copy Output / Select Output | `Ctrl+Shift+Y` / `Ctrl+Shift+G` / `Ctrl+Shift+A` |
| Bookmark / Prev / Next / Search Bookmarks | `Ctrl+Shift+B` / `Ctrl+Shift+K` / `Ctrl+Shift+J` / `Ctrl+Alt+Shift+B` |
| Re-run Last Command | `Ctrl+Shift+R` |
| Zoom In / Out / Reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Open Working Directory / Reload Config | `Ctrl+Shift+O` / `Ctrl+Shift+,` |

## License

Lune is licensed under the [MIT License](LICENSE).
