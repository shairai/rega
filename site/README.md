# Rega website

The static site at **https://rega-capture.vercel.app** (the address is
`SITE_URL` in `src/config.rs`): the home page, privacy, and
the installers. Plain HTML and CSS with no build step, hosted free on Vercel's
Hobby plan.

| File | What it is |
|---|---|
| `index.html`, `privacy.html`, `404.html` | The pages. Edit them by hand, except the `<!-- generated:… -->` blocks. |
| `styles.css`, `site.js` | Styles (the app's colours and Ubuntu font); `site.js` picks the visitor's system for the download button and runs the demo video. |
| `consent.js` | The cookie banner, and Google Tag Manager once the visitor accepts. See [Analytics](#analytics). |
| `images/`, `fonts/` | Screenshots from `docs/images` as WebP, and the Ubuntu font as WOFF2 subsets. |
| `media/demo.mp4`, `media/demo.jpg` | The home page's demo video and its poster. Rendered from the real UI, see below. |
| `guide-demo.html` | A real guide page, exported by Rega's own exporter, shown in a frame in the Guides section. See below. |
| `latest.json` | **Read by the app** once a day to see if a newer version is out. |
| `vercel.json` | Headers, and the `/download/mac` and `/download/windows` redirects. |
| `downloads/` | The current installers. Git-ignored: only Vercel gets them. |

`latest.json`, the redirects and the generated blocks are
written by `scripts/build-site.py` from `CHANGELOG.md`, `Cargo.toml` and the
installers. Don't edit them by hand.

## Releasing a version

```sh
# 1. Bump `version` in Cargo.toml, then in CHANGELOG.md rename
#    "## Unreleased" to "## 0.4.2" (the script adds the date).
# 2. Build both installers. On the Mac with the "Developer ID Application"
#    certificate and the rega-notary profile (see package-macos.sh), the app
#    and DMG are signed, notarized and stapled; publish-site.sh refuses a
#    DMG that isn't. Run it in Terminal: the first signing asks for keychain
#    access (click Always Allow).
./scripts/package-macos.sh
./scripts/package-windows-cross.sh
# 3. Copy them into site/downloads, regenerate the site, deploy, and check
#    the live latest.json and download links.
./scripts/publish-site.sh
# 4. Commit CHANGELOG.md and site/.
```

Within a day, every running Rega with update checks on shows *"Rega 0.4.2 is
available"* with a **Download** button that opens the site's downloads
(`/#download`; Rega 0.4.2 and 0.4.3 open `/releases`, which redirects there).
The release notes stay in `CHANGELOG.md`; the site shows only the latest
version's one-line summary, in the "New" pill at the top of the home page.

To look at the site before deploying, run `./scripts/publish-site.sh --no-deploy`
and serve the folder, e.g. `npx serve site` (which also honours `cleanUrls`).
To try the app's notice against it, start Rega with
`REGA_UPDATE_URL=http://localhost:3000/latest.json` and a higher version in
that file.

## The demo video

The video at the top of the home page is the real app, rendered off-screen
the same way as the README screenshots. It presses ⌃⌥C, drags over a demo
dashboard, adds an arrow, a callout, a blur, step numbers and a stamp with
their one-key tools, copies, and pastes into a chat. The pointer, click
rings and key badges are drawn on top. After a UI change, render it again
(needs `ffmpeg`):

```sh
cargo test --release demo_video -- --ignored
```

This rewrites `media/demo.mp4` (about 1 MB) and `media/demo.jpg`, and sets
the `data-t` times of the four steps under the video in `index.html`. The
script is `src/app/readme_shots/demo.rs`.

## Analytics

`consent.js` asks each visitor once whether the site may count visits.
Nothing from Google loads until they click **Accept**. Then it loads Google
Tag Manager (container `GTM-5Q9HZX6Z`, set at the top of the file) with
Consent Mode's `analytics_storage` granted. **Cookie settings** in every
footer reopens the banner; Decline turns analytics off and removes the
`_ga` cookies.

Each click on a download button pushes this to the data layer:

```js
{ event: "rega_download", download_os: "mac" | "windows", download_version: "0.4.2",
  download_placement: "hero" | "download-card", download_page: "/" }
```

The container needs, once:

1. **Google tag**: your GA4 measurement ID (`G-…`, from GA4 Admin › Data
   streams › Web), trigger *Initialization – All Pages*. This counts visits.
2. **Data Layer Variables** `download_os`, `download_version`,
   `download_placement`, `download_page`.
3. **Trigger**: Custom Event, event name `rega_download`.
4. **GA4 Event tag**: event name `file_download`, parameters `os`, `version`,
   `placement` and `page` from those variables, on that trigger.
5. **Submit** (publish) the container.

In GA4, register `os`, `version` and `placement` under Admin › Custom
definitions (event-scoped) to break `file_download` down by them in
Reports › Engagement › Events. Only visitors who accepted are counted, so
downloads are a lower bound.

## The demo guide

The Guides section frames `guide-demo.html`: a five-step guide, *Capture,
mark up and share a screenshot*, exported exactly the way Rega exports one
(`src/guide/export.rs`). Each step's screenshot is the real UI rendered
off-screen, with the marker on the control that step uses. After a UI or
guide-page change, render it again:

```sh
cargo test --release site_guide -- --ignored
```

The site allows framing its own pages only (`X-Frame-Options: SAMEORIGIN`
in `vercel.json`), which the frame needs.

## First deploy

1. `npm i -g vercel` and `vercel login` (a free Hobby account).
2. `./scripts/publish-site.sh`. The first run links this folder to the Vercel
   project `rega-capture`, creating it if needed, so the site lands on
   `rega-capture.vercel.app`.

Deploy with the script rather than connecting the GitHub repository to
Vercel. A Git-connected project would redeploy on every push **without** the
installers (they aren't in git), and the download links would break.

## Moving the site

For another `*.vercel.app` name or your own domain, change `SITE_URL` in
`src/config.rs` (and `VERCEL_PROJECT` when running the script, if the project
name differs). Do it before a release: each Rega build checks the address it
was built with, so older versions keep asking the old one. Keep the old
address serving `latest.json` (or redirecting) for a while after a move.

## Costs

Nothing on the Hobby plan: 100 GB of transfer a month is about 4,500
downloads of both installers together. If downloads ever approach that, move
the installers to GitHub Releases or Cloudflare R2 and point the redirects in
`build-site.py` at them.
