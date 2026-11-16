# Rega website

The static site at **https://rega-capture.vercel.app** (the address is
`SITE_URL` in `src/config.rs`): the home page, **What's new**, privacy, and
the installers. Plain HTML and CSS with no build step, hosted free on Vercel's
Hobby plan.

| File | What it is |
|---|---|
| `index.html`, `releases.html`, `privacy.html`, `404.html` | The pages. Edit them by hand, except the `<!-- generated:… -->` blocks. |
| `styles.css`, `site.js` | Styles (the app's colours and Ubuntu font); `site.js` only picks the visitor's system for the download button. |
| `images/`, `fonts/` | Screenshots from `docs/images` as WebP, and the Ubuntu font as WOFF2 subsets. |
| `latest.json` | **Read by the app** once a day to see if a newer version is out. |
| `releases.json` | Every version and its notes. |
| `vercel.json` | Headers, and the `/download/mac` and `/download/windows` redirects. |
| `downloads/` | The current installers. Git-ignored: only Vercel gets them. |

`latest.json`, `releases.json`, the redirects and the generated blocks are
written by `scripts/build-site.py` from `CHANGELOG.md`, `Cargo.toml` and the
installers. Don't edit them by hand.

## Releasing a version

```sh
# 1. Bump `version` in Cargo.toml, then in CHANGELOG.md rename
#    "## Unreleased" to "## 0.4.2" (the script adds the date).
# 2. Build both installers.
./scripts/package-macos.sh
./scripts/package-windows-cross.sh
# 3. Copy them into site/downloads, regenerate the site, deploy, and check
#    the live latest.json and download links.
./scripts/publish-site.sh
# 4. Commit CHANGELOG.md and site/.
```

Within a day, every running Rega with update checks on shows *"Rega 0.4.2 is
available"* with a **Download** button that opens `/releases`.

To look at the site before deploying, run `./scripts/publish-site.sh --no-deploy`
and serve the folder, e.g. `npx serve site` (which also honours `cleanUrls`).
To try the app's notice against it, start Rega with
`REGA_UPDATE_URL=http://localhost:3000/latest.json` and a higher version in
that file.

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
