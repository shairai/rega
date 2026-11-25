# Changelog

What's new in each version of Rega. The website's download section and the
in-app update notice (`latest.json`) are built from this file by
`scripts/publish-site.sh`:

- One `## <version> — <YYYY-MM-DD>` heading per release, newest first. Leave
  the date out and the script fills in the day you publish.
- An optional one-line summary under the heading, then `- ` bullets.
- `**bold**` and `` `code` `` work in the text.
- `## Unreleased` collects changes for the next version; the site skips it.

## 0.4.15 — 2026-11-25

Guides: mark up the pictures, hand a guide over to edit, pause on a click.

- **Guides: mark up a step's picture.** *Mark up picture* opens the step's
  screenshot in the image editor, with every tool there is: arrows, text,
  blur for anything private, crop. *Done* puts it back in the step, and
  the marks stay editable.
- **Shared guides pause on a click**: click the picture to stop on a step
  and look, click again to carry on. (Its edges still go back and on.)
  Guides made before get this the next time they are saved.
- **Guides: export to edit.** *Export › Editable guide* saves the whole
  guide as one `.regaguide` file, with its full-quality pictures and the
  marks on them. Whoever gets it double-clicks it: it opens in their Rega
  (which now opens `.regaguide` files, on Mac and Windows) to edit before
  it is published. *File › Open Guide…* (`⇧⌘O`), *Open image…* and
  dropping it on the window work too.
- Guides: the step list follows a title as it is typed.

## 0.4.14 — 2026-11-24

Scrolling captures that keep going, no more flashes, a lighter editor.

- **Scrolling capture** no longer stops after one scroll on a page with a
  video or an animation on it: frames are lined up by where most of the
  page agrees, and each scroll is given time to come to rest.
- Mac: the screen no longer flashes when a capture or a color pick starts
  or ends (a black frame as it began, Rega's window over the whole screen
  for a moment as it ended).
- A mark drawn, placed, typed or moved past the edge of the picture now
  gets canvas under it, as pasted pictures did. (Blur, fill, magnifier and
  spotlight don't: they need picture to work on.)
- **Select all** (`⌘A` / `Ctrl+A`) lets go of the selected mark and picks
  the whole picture, so Copy takes all of it.
- Faster and lighter on large images: the picture is redrawn two to three
  times faster after a blur, fill or magnifier changes, and the undo
  history no longer grows without limit on long captures.

## 0.4.13 — 2026-11-23

Select and move part of a picture, resize the canvas, one Shape tool.

- **Select area**, a new tool (`G`): drag over part of the picture, then
  drag it somewhere else. It lifts off as a picture you can move, resize,
  turn or delete; hold `⌥` / `Alt` to drag a copy. `⌘X` cuts the area,
  `⌘C` copies it, `⌫` deletes it.
- **Change the canvas size** by dragging the handles on the image's edge:
  outward to add space around the picture, inward to trim it.
- Rectangle and Ellipse are one tool now, **Shape**: pick which in the
  side panel, where a selected shape can also be turned into the other.
  `R` and `E` still pick them.
- **New** starts at the size of the image on the clipboard, when there is
  one, so it can be pasted onto a canvas that fits it.
- A pasted image keeps its own size whenever the canvas can hold it; it
  used to shrink to 80% of the canvas.
- Marks reaching past the canvas edge are cut off there on screen, as they
  are in the saved image.
- A copied mark also goes on the clipboard as a picture, so it can be
  pasted into a new image (which opens at its size) or another app.
- `⌘X` / `Ctrl+X` also cuts a selected mark.

## 0.4.12 — 2026-11-23

Fixes a crash with more than one screen.

- Mac: with two or more screens, Rega could quit during a capture or as
  it ended — on slower display setups (a dock, say) almost every time.
  This was in earlier versions too, and 0.4.11 fixed only part of it.

## 0.4.11 — 2026-11-23

Fixes a crash with more than one screen.

- Mac: with two or more screens, Rega 0.4.10 could quit when a capture
  ended or was cancelled. Fixed.
- If Rega ever does quit unexpectedly, it now notes why in `crash.log` in
  its settings folder, to send along with a bug report.

## 0.4.10 — 2026-11-23

Captures start sooner, and Windows updates install.

- Mac: the capture overlay comes up about twice as fast when Rega is
  waiting in the menu bar — no more pause after the shortcut.
- Windows: the installer now closes every running copy of Rega first, so
  **Update now** isn't stopped by a second copy left in the tray (the
  update could end with the old version starting again).

## 0.4.9 — 2026-11-20

Rotate anything, and bring images in however you like.

- **Rotate** a selected mark: drag the round handle above it (hold Shift
  for 15° steps), or use **Rotation** in the side panel to type an angle,
  turn it 90° or stand it upright again. Shapes, lines, pen strokes, text,
  pasted pictures and stamps turn.
- **Rotate the whole image** a quarter turn left or right from the new
  **Rotate** button; everything on it turns along.
- **Drag images from Finder or Explorer** onto Rega: onto the library to
  open them, onto an open image to add them as layers.
- Pasting files copied in Finder now pastes the pictures, not their file
  icons, and pastes every one you copied, not only the first. **Open
  image…** also opens TIFF and ICO files.
- `⌘W`, `⌘Q` and the other shortcuts work with a Hebrew keyboard layout.

## 0.4.8 — 2026-11-17

Pick a color from anything on screen.

- **Pick a color** from anything on screen: `⌃⌥P` (`Ctrl+Alt+P` on Windows)
  puts a loupe on the pointer over every screen; click a pixel and its hex
  code (like `#AE4E4A`) is on your clipboard, without opening Rega. The
  shortcut can be changed in Settings › Shortcuts. It's also on Rega's home
  screen, its toolbar and the menu bar.

## 0.4.7 — 2026-11-17

Update in place, with one click.

- Updates in place: the update notice's **Update now** saves your work,
  downloads the new version, checks it (its checksum, and on macOS that it's
  signed by the same developer) and restarts into it. Where Rega can't
  replace itself (run from the disk image, say) it opens the download page.

## 0.4.6 — 2026-11-17

Every screen at once, and edits that save themselves.

- More than one screen: the capture overlay now covers every screen at
  once. Select, click a window or press Enter on whichever screen you like;
  **Screen** lets you click the screen to take.
- Edits save themselves a moment after you stop, and the library's
  preview follows, sharper than before. The **Save** button is gone (`⌘S` /
  `Ctrl+S` still saves at once).
- The editor opens with the tool you used last.
- Highlighter: starts 8 thick and keeps its own thickness; hold Shift to
  stay on the line you started on.
- Guides: "Made with Rega" links to the Rega website.

## 0.4.5 — 2026-11-17

Capture on any screen, and copy several captures at once.

- More than one screen: the capture overlay follows the mouse, so move it to
  the screen you want and select there. **Screen** asks which screen to take:
  click the one you want.
- Library: select several captures (`⌘`-click or Shift-click) and copy them
  all with **Copy**, the right-click menu or `⌘C` / `Ctrl+C`. They paste into
  chats, emails and folders as files.
- New image: clearer sizes, width and height boxes you can type in, and a
  padlock that keeps the ratio.
- Guides: the page settings fit their panel again, and **Marker size** no
  longer hides behind its buttons.

## 0.4.4 — 2026-11-17

A new look for the color picker.

- Editor: the eyedropper's loupe now sits right on the pointer: a round,
  enlarged view of the pixels under it, the one a click picks outlined in
  the middle, and its hex code (like `#AE4E4A`) just below.
- Screen readers now name the capture buttons on the home screen.

## 0.4.3 — 2026-11-16

Signed and notarized on macOS.

- macOS: Rega is signed with a Developer ID and notarized by Apple, so it
  opens like any other app — no more **Open Anyway** in System Settings.
- If macOS asks for Screen Recording or Accessibility again after this
  update, allow them once more; later updates keep them.

## 0.4.2 — 2026-11-16

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
