#!/usr/bin/env bash
# Put the current version on the website and deploy it to Vercel.
#
#   1. Bump the version in Cargo.toml and write its notes in CHANGELOG.md
#      (rename "## Unreleased" to "## <version>").
#   2. Build both installers:
#        ./scripts/package-macos.sh
#        ./scripts/package-windows-cross.sh
#   3. ./scripts/publish-site.sh              update site/ and deploy it
#      ./scripts/publish-site.sh --no-deploy  only update site/ (preview with
#                                             `npx serve site` or similar)
#   4. Commit CHANGELOG.md and site/ (the installers in site/downloads/ stay
#      out of git: only Vercel gets them).
#
# Once, before the first deploy: `npm i -g vercel` and `vercel login`. The
# first run links site/ to the Vercel project $VERCEL_PROJECT (created if
# needed), whose address must match SITE_URL in src/config.rs.
set -euo pipefail
cd "$(dirname "$0")/.."

DEPLOY=1
[[ "${1:-}" == "--no-deploy" ]] && DEPLOY=0
VERCEL_PROJECT="${VERCEL_PROJECT:-rega-capture}"

VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
SITE_URL=$(sed -n 's/^pub const SITE_URL: &str = "\(.*\)";/\1/p' src/config.rs)
MAC="Rega-${VERSION}-macos-universal.dmg"
WIN="Rega-${VERSION}-windows-x64-setup.exe"

for f in "$MAC" "$WIN"; do
  if [[ ! -f "dist/$f" ]]; then
    echo "Missing dist/$f — build it first:"
    echo "  ./scripts/package-macos.sh && ./scripts/package-windows-cross.sh"
    exit 1
  fi
done

# Only the current version is hosted; older installers are dropped.
rm -rf site/downloads
mkdir -p site/downloads
cp "dist/$MAC" "dist/$WIN" site/downloads/

python3 scripts/build-site.py

if [[ $DEPLOY == 0 ]]; then
  echo "site/ is ready (not deployed)."
  exit 0
fi

if ! command -v vercel >/dev/null; then
  echo "The Vercel CLI isn't installed: npm i -g vercel && vercel login"
  exit 1
fi

cd site
if [[ ! -f .vercel/project.json ]]; then
  # The project's name is its address: <name>.vercel.app.
  vercel link --yes --project "$VERCEL_PROJECT" \
    || { vercel project create "$VERCEL_PROJECT" && vercel link --yes --project "$VERCEL_PROJECT"; }
fi
vercel deploy --prod --yes

# Check what visitors and the app will get. The address can take a few
# seconds to switch to the new deployment, so keep asking for a minute.
echo "Checking ${SITE_URL}…"
LIVE=""
for _ in $(seq 12); do
  LIVE=$(curl -fsS "${SITE_URL}/latest.json" 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])' 2>/dev/null || true)
  [[ "$LIVE" == "$VERSION" ]] && break
  sleep 5
done
if [[ "$LIVE" != "$VERSION" ]]; then
  echo "${SITE_URL}/latest.json says '${LIVE:-nothing}', expected ${VERSION}."
  echo "Try again in a minute; if it stays wrong, check that SITE_URL in src/config.rs"
  echo "matches the project's domain in the Vercel dashboard."
  exit 1
fi
for slug in mac windows; do
  curl -fsSIL -o /dev/null "${SITE_URL}/download/${slug}" || { echo "${SITE_URL}/download/${slug} doesn't download"; exit 1; }
done
echo "Live: ${SITE_URL} — Rega ${VERSION}"
