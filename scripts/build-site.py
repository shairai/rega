#!/usr/bin/env python3
"""Fill the website (site/) with the current release.

Reads the version from Cargo.toml, release notes from CHANGELOG.md, the site's
address from src/config.rs (SITE_URL) and the installers in site/downloads/,
then writes:

  site/latest.json     what the app's update check reads
  site/vercel.json     /download/mac and /download/windows point at the files,
                       and /releases (opened by Rega 0.4.2-0.4.3's update
                       notice) at the downloads
  site/*.html          the blocks between <!-- generated:NAME --> markers

Run by scripts/publish-site.sh; safe to run again (the output only changes
when its inputs do). Standard library only, Python 3.9+.
"""

import datetime
import html
import json
import pathlib
import re
import sys

GITHUB_URL = "https://github.com/shairai/rega"

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
DOWNLOADS = SITE / "downloads"

PLATFORMS = [
    # latest.json key, /download/<slug>, installer name (as the package scripts write it)
    ("macos", "mac", "Rega-{v}-macos-universal.dmg"),
    ("windows", "windows", "Rega-{v}-windows-x64-setup.exe"),
]


def fail(msg):
    sys.exit(f"build-site: {msg}")


def cargo_version():
    m = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    if not m:
        fail("no version in Cargo.toml")
    return m.group(1)


def site_url():
    m = re.search(r'pub const SITE_URL: &str = "([^"]+)";', (ROOT / "src/config.rs").read_text())
    if not m:
        fail("no SITE_URL in src/config.rs")
    return m.group(1).rstrip("/")


# ---------- CHANGELOG.md ----------

HEADING = re.compile(r"^## +(\d+\.\d+\.\d+)(?: +[—–-] +(\d{4}-\d{2}-\d{2}))? *$", re.M)


def stamp_date(version):
    """Give the release's heading today's date if it has none; return the text."""
    path = ROOT / "CHANGELOG.md"
    text = path.read_text()
    m = next((m for m in HEADING.finditer(text) if m.group(1) == version), None)
    if not m:
        fail(f"CHANGELOG.md has no '## {version}' section. Write the release notes first "
             f"(rename '## Unreleased' to '## {version}').")
    if not m.group(2):
        today = datetime.date.today().isoformat()
        text = text[: m.start()] + f"## {version} — {today}" + text[m.end():]
        path.write_text(text)
        print(f"CHANGELOG.md: dated {version} {today}")
    return text


def parse_changelog(text):
    """[{version, date, title, notes}] newest first, from the dated sections."""
    releases = []
    matches = list(HEADING.finditer(text))
    for i, m in enumerate(matches):
        body = text[m.end(): matches[i + 1].start() if i + 1 < len(matches) else len(text)]
        # Stop at the next heading of any kind (e.g. "## Unreleased" can't follow, but be safe).
        body = re.split(r"^## ", body, flags=re.M)[0]
        title, notes = [], []
        for line in body.splitlines():
            if line.startswith("- "):
                notes.append(line[2:].strip())
            elif line.startswith("  ") and notes:
                notes[-1] += " " + line.strip()
            elif line.strip() and not notes:
                title.append(line.strip())
        if m.group(2):
            releases.append({
                "version": m.group(1),
                "date": m.group(2),
                "title": " ".join(title),
                "notes": notes,
            })
    releases.sort(key=lambda r: tuple(int(x) for x in r["version"].split(".")), reverse=True)
    return releases


def plain(md):
    return re.sub(r"\*\*(.+?)\*\*|`([^`]+)`", lambda m: m.group(1) or m.group(2), md)


def nice_date(iso):
    d = datetime.date.fromisoformat(iso)
    return f"{d.day} {d.strftime('%b %Y')}"


def megabytes(size):
    return f"{size / 1_000_000:.1f} MB"


# ---------- HTML blocks ----------

APPLE = ('<path d="M16.37 12.73c-.02-2.13 1.74-3.15 1.82-3.2-.99-1.45-2.53-1.65-3.08-1.67-1.31-.13-2.56.77-3.22.77'
         '-.67 0-1.69-.75-2.78-.73-1.43.02-2.75.83-3.48 2.11-1.49 2.58-.38 6.39 1.07 8.48.71 1.02 1.55 2.17 2.66 2.13'
         ' 1.07-.04 1.47-.69 2.76-.69s1.65.69 2.78.67c1.15-.02 1.88-1.04 2.58-2.07.81-1.19 1.15-2.34 1.17-2.4'
         '-.03-.01-2.24-.86-2.26-3.4M14.25 6.47c.59-.71.98-1.7.87-2.69-.85.03-1.88.57-2.49 1.28-.55.63-1.03 1.64'
         '-.9 2.61.95.07 1.92-.48 2.52-1.2"/>')
WINDOWS = '<path d="M3 5.1 10.4 4v7.1H3zm8.3-1.2L21 2.5v8.6h-9.7zM3 12h7.4v7.1L3 18zm8.3 0H21v8.6l-9.7-1.4z"/>'

CARDS = {
    "macos": {
        "slug": "mac",
        "icon": APPLE,
        "name": "macOS",
        "note": "Apple silicon and Intel",
        "button": "Download for Mac",
        "kind": "disk image",
        "needs": "macOS 12.3 or later",
        "steps": [
            "Open the disk image and drag Rega to Applications.",
            "Open Rega. It's signed and notarized by Apple, so it opens like any other app.",
            "Grant Screen Recording (and Accessibility for scrolling captures and guides) when Rega asks.",
        ],
    },
    "windows": {
        "slug": "windows",
        "icon": WINDOWS,
        "name": "Windows",
        "note": "64-bit installer",
        "button": "Download for Windows",
        "kind": "installer",
        "needs": "Windows 10 or 11, 64-bit",
        "steps": [
            "Run the installer. If Windows SmartScreen says it protected your PC, click <b>More info</b> › "
            "<b>Run anyway</b>: the installer isn't code-signed yet.",
            "Rega starts in the system tray, next to the clock.",
            "On Windows N editions, MP4 recording needs Microsoft's free Media Feature Pack.",
        ],
    },
}


def downloads_html(latest, files):
    cards = []
    for key, card in CARDS.items():
        steps = "\n".join(f"        <li>{s}</li>" for s in card["steps"])
        cards.append(f"""  <article class="download" data-os="{card['slug']}">
    <header>
      <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">{card['icon']}</svg>
      <div><h3>{card['name']}</h3><div class="os-note">{card['note']}</div></div>
      <span class="you-tag">Your system</span>
    </header>
    <a class="btn btn-primary btn-large" href="/download/{card['slug']}" data-version="{latest['version']}">{card['button']}</a>
    <dl><dt>Version</dt><dd>{latest['version']} · {nice_date(latest['date'])}</dd><dt>Size</dt><dd>{megabytes(files[key]['size'])} {card['kind']}</dd><dt>Needs</dt><dd>{card['needs']}</dd></dl>
    <details>
      <summary>First launch</summary>
      <ol>
{steps}
      </ol>
    </details>
  </article>""")
    return '<div class="downloads">\n' + "\n".join(cards) + "\n</div>"


def attr(text):
    return html.escape(text, quote=False).replace('"', "&quot;")


def head_html(url, path, title, description):
    page = url + path
    return "\n".join([
        f'<link rel="canonical" href="{page}">',
        '<meta property="og:type" content="website">',
        f'<meta property="og:url" content="{page}">',
        f'<meta property="og:title" content="{attr(title)}">',
        f'<meta property="og:description" content="{attr(description)}">',
        f'<meta property="og:image" content="{url}/images/og.jpg">',
        '<meta name="twitter:card" content="summary_large_image">',
    ])


def fill(path, blocks):
    text = path.read_text()
    for name, content in blocks.items():
        pattern = re.compile(
            rf"(?P<indent>[ \t]*)<!-- generated:{name} -->\n.*?(?P=indent)<!-- /generated:{name} -->", re.S)
        if not pattern.search(text):
            fail(f"{path.relative_to(ROOT)} has no <!-- generated:{name} --> block")
        def replace(m):
            ind = m.group("indent")
            body = "\n".join(ind + line if line.strip() else line for line in content.splitlines())
            return f"{ind}<!-- generated:{name} -->\n{body}\n{ind}<!-- /generated:{name} -->"
        text = pattern.sub(replace, text, count=1)
    path.write_text(text)


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


# ---------- main ----------

def main():
    version = cargo_version()
    url = site_url()
    releases = parse_changelog(stamp_date(version))
    if not releases or releases[0]["version"] != version:
        newest = releases[0]["version"] if releases else "none"
        fail(f"Cargo.toml says {version} but the newest dated CHANGELOG.md section is {newest}")
    latest = releases[0]

    files = {}
    for key, slug, pattern in PLATFORMS:
        name = pattern.format(v=version)
        f = DOWNLOADS / name
        if not f.is_file():
            fail(f"missing site/downloads/{name} (scripts/publish-site.sh copies it from dist/)")
        files[key] = {"name": name, "size": f.stat().st_size}

    write_json(SITE / "latest.json", {
        "version": version,
        "date": latest["date"],
        "title": plain(latest["title"]),
        "notes": [plain(n) for n in latest["notes"]],
        "url": f"{url}/#download",
        "downloads": {
            key: {"url": f"{url}/downloads/{f['name']}", "size": f["size"]} for key, f in files.items()
        },
    })

    vercel = json.loads((SITE / "vercel.json").read_text())
    vercel["redirects"] = [
        {"source": f"/download/{slug}", "destination": f"/downloads/{files[key]['name']}", "permanent": False}
        for key, slug, _ in PLATFORMS
    ] + [
        {"source": "/download", "destination": "/#download", "permanent": False},
        {"source": "/releases", "destination": "/#download", "permanent": False},
    ]
    write_json(SITE / "vercel.json", vercel)

    pill_title = plain(latest["title"]).rstrip(".") or "see what's new"
    fill(SITE / "index.html", {
        "head": head_html(url, "/", "Rega — capture anything, mark it up, share it in seconds",
                          "Screenshots, scrolling captures, screen recordings, step-by-step guides and OCR "
                          "for macOS and Windows. Free."),
        "pill": f'<a class="pill" href="#download"><b>New</b> Rega {version} — '
                f"{html.escape(pill_title)} →</a>",
        "hero-meta": f'<p class="hero-meta">Version {version} · macOS 12.3+ and Windows 10/11 · '
                     f'<a href="{GITHUB_URL}">Open source on GitHub</a></p>',
        "downloads": downloads_html(latest, files),
    })
    print(f"site/: Rega {version} ({latest['date']}), {url}")


if __name__ == "__main__":
    main()
