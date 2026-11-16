# Changelog

What's new in each version of Rega. The website's download section, its
**What's new** page and the in-app update notice are built from this file by
`scripts/publish-site.sh`:

- One `## <version> — <YYYY-MM-DD>` heading per release, newest first. Leave
  the date out and the script fills in the day you publish.
- An optional one-line summary under the heading, then `- ` bullets.
- `**bold**` and `` `code` `` work in the text.
- `## Unreleased` collects changes for the next version; the site skips it.

## 0.4.2

Update notices, and a steadier window on Windows.

- Rega tells you when a new version is out: it checks the website once a day
  and shows a notice with a **Download** button. Turn it off in Settings ›
  General › Updates.
- Windows: Rega draws with DirectX 12 on the graphics card that drives your
  screen, to stop the window blinking blank while you hover the capture
  buttons on some PCs.

## 0.4.1 — 2026-11-15

Copy and paste marks in the editor.

- Select any mark (arrow, text, shape, stamp, pasted image…) and press `⌘C` /
  `Ctrl+C`, then `⌘V` / `Ctrl+V` to drop a copy next to it. Paste again for
  more copies, each a little further along.
- Copying a pasted image also puts it on the system clipboard, so other apps
  can take it.

## 0.4.0 — 2026-11-11

Guides: do a task once, get a step-by-step how-to.

- **Guide** (`⌃⌥G`) turns every click into a step with a screenshot and a
  marker, typing in a field into one step, and shortcuts into steps of their
  own.
- Reorder steps by dragging, give each a title and description, move or hide
  its marker.
- Share one small HTML page that plays like stories, works offline and on
  phones, and follows the viewer's light or dark mode.
- Settings › Guide: marker style and colour, step time, quality, and what
  recording picks up.
- Step numbers in the editor can start at any number, or **Continue at N**
  from another image.
- The highlighter keeps its own yellow and straightens wobbly strokes.
- The selection and recording border turns in the logo's gradient, with
  rounded corners.

## 0.3.0 — 2026-11-05

Faster recording on the system's own video codec.

- MP4 recording, playback and trimming use the H.264 codec built into macOS
  and Windows, so the app is smaller.
- Frames are scaled 6–9× faster, and a Retina full-screen recording at 30 fps
  fills every frame.
- GIFs encode up to 2× faster; long GIFs open instantly in the player.
- Windows: Rega starts on N editions too, and points you to the Media Feature
  Pack for MP4.

## 0.2.0 — 2026-11-04

Windows, a video player, and more tools.

- Windows 10 and 11 (64-bit), with a branded installer.
- A video player with trimming and **Save as GIF**.
- GIF quality presets (Small, Balanced, High) and much lighter GIFs.
- New tools: magnifier, stamps, fill and eraser, plus **Flatten image**.
- A properties panel that shows only what applies to the tool or mark.
- Select several library items with `⌘`/`⇧`-click; an About window.
- Recording is sturdier: quitting mid-recording keeps the video, and a full
  disk no longer crashes Rega.

## 0.1.0 — 2026-11-02

First preview for macOS.

- Region, window and full-screen capture with a pixel magnifier.
- The annotation editor: arrows, shapes, text, blur, steps, crop and more.
- Scrolling capture, screen recording to MP4 or GIF, and grab text (OCR).
- A library of every capture, global shortcuts and a menu bar icon.
