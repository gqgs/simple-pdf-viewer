# Simple PDF Viewer

A focused Linux PDF reader built with Rust, eframe/egui, Poppler, and Cairo.

It provides single-page reading, direct page navigation, responsive zoom controls, text highlights with attached notes, internal-link previews, and Back/Forward navigation after following references.

## Requirements

- Rust 1.95 or newer
- `pkg-config`
- Poppler GLib and Cairo development files

Install the native dependencies with one of:

```sh
# Arch Linux
sudo pacman -S poppler-glib pkgconf

# Debian / Ubuntu
sudo apt install libpoppler-glib-dev libcairo2-dev pkg-config

# Fedora
sudo dnf install poppler-glib-devel cairo-devel pkgconf-pkg-config
```

## Run

Open the file picker:

```sh
cargo run --release
```

Or open a PDF immediately:

```sh
cargo run --release -- sample.pdf
```

You can also drop a PDF into the application window.

## Controls

| Action | Control |
| --- | --- |
| Open PDF | `Ctrl+O` |
| Previous / next page | `Page Up` / `Page Down` |
| First / last page | `Home` / `End` |
| Return after following a reference | `Alt+Left` or **Back** |
| Go forward again | `Alt+Right` or **Forward** |
| Zoom | `Ctrl+mouse wheel`, `Ctrl++`, or `Ctrl+-` |
| Reset zoom | `Ctrl+0` |

Enter a page number in the toolbar to jump directly to it. Zoom can also be typed as a percentage, or changed to **Fit width** or **Fit page**.

Drag across text and choose **Highlight** or **Add note**. Highlights and notes are saved automatically beside the document as `<document>.pdf.spv.json`; the original PDF is never changed.

Hover over an internal PDF link to preview the destination. Clicking it navigates to the destination while preserving the originating page, scroll position, and zoom for the Back button. External URI links open in the system browser.

## Current limitations

- Linux desktop only
- One open document at a time
- Password-protected PDFs are not supported
- Text selection requires an embedded text layer; OCR is not included
- Highlights are viewer sidecars, not annotations embedded in the PDF

