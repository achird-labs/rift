#!/usr/bin/env bash
#
# Docs internal-link gate (issue #898).
#
# Resolves every internal link in the *built* docs site the way GitHub Pages actually serves it,
# and fails if any of them would 404. This is the gate that was missing when the whole site
# shipped with 156 dead cross-references: `docs/_config.yml` emitted `features/spaces.html` while
# every hand-written link pointed at `features/spaces/`, and nothing noticed — the just-the-docs
# navigation is generated from `page.url`, so the theme's own links stayed correct and the site
# looked navigable. Only links a human wrote were broken.
#
# Checking the *sources* would not have caught it: `{{ site.baseurl }}/features/spaces/` is a
# perfectly well-formed link. The bug only exists in the relationship between the authored URL and
# the file layout Jekyll produces, so the check has to run against the built output.
#
# Pages' resolution rules, which this reproduces:
#   - `/path/`      -> requires `path/index.html`
#   - `/path`       -> `path`, `path.html`, or `path/index.html` (the last via a 301 to `/path/`)
#   - `/path.html`  -> requires that exact file
#
# Scope: internal links only. External URLs are not fetched (flaky, and not this gate's job), and
# `#fragment` targets are not resolved — this catches dead *pages*, not dead anchors.
#
# Usage:
#   scripts/verify-docs-links.sh [site-dir]     # check a built site (default: _site)
#   scripts/verify-docs-links.sh --self-test    # prove the checker flags the #898 layout
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Overridable so the self-test can point the checker at fixtures.
BASEURL="${BASEURL:-$(grep -E '^baseurl:' "$repo_root/docs/_config.yml" | sed -E 's/^baseurl:[[:space:]]*"?([^"]*)"?[[:space:]]*$/\1/')}"

check_site() {
  local site_dir="$1" baseurl="$2"
  SITE_DIR="$site_dir" BASEURL="$baseurl" python3 - <<'PY'
import os, re, sys
from html.parser import HTMLParser
from urllib.parse import unquote, urljoin

site = os.path.abspath(os.environ["SITE_DIR"])
baseurl = os.environ["BASEURL"].rstrip("/")

if not os.path.isdir(site):
    sys.exit(f"verify-docs-links: site dir not found: {site}\nBuild it first (see .github/workflows/ci.yml).")

EXTERNAL = re.compile(r"^(?:[a-z][a-z0-9+.-]*:|//)", re.I)


class Links(HTMLParser):
    """Collect href/src targets. Regex over HTML misses attribute quoting edge cases."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.found = []

    def handle_starttag(self, tag, attrs):
        attr = {"a": "href", "link": "href", "img": "src", "script": "src"}.get(tag)
        if not attr:
            return
        for k, v in attrs:
            if k == attr and v:
                self.found.append(v)


def resolves(path: str) -> bool:
    """Does `path` (site-root-relative, already stripped of baseurl) resolve on GitHub Pages?"""
    target = os.path.normpath(os.path.join(site, path.lstrip("/")))
    # normpath collapses `..`; a link that escapes the site root can never resolve.
    if not (target == site or target.startswith(site + os.sep)):
        return False
    if path.endswith("/"):
        # Trailing slash is served *only* by a directory index. This is the #898 case:
        # `features/spaces.html` exists but `features/spaces/` is a 404.
        return os.path.isfile(os.path.join(target, "index.html"))
    return (
        os.path.isfile(target)
        or os.path.isfile(target + ".html")
        or os.path.isfile(os.path.join(target, "index.html"))
    )


broken = []
pages = 0
for root, _, files in os.walk(site):
    for name in files:
        if not name.endswith(".html"):
            continue
        pages += 1
        page = os.path.join(root, name)
        # The URL this file is served at, so relative links resolve from the right place.
        page_url = "/" + os.path.relpath(page, site).replace(os.sep, "/")

        parser = Links()
        with open(page, encoding="utf-8", errors="replace") as fh:
            parser.feed(fh.read())

        for href in parser.found:
            href = href.strip()
            if not href or href.startswith("#") or EXTERNAL.match(href):
                continue
            target = href.split("#")[0].split("?")[0]
            if not target:
                continue
            if target.startswith("/"):
                # Absolute: written with the baseurl, which is not part of the file layout.
                if baseurl:
                    if target == baseurl:
                        target = "/"
                    elif target.startswith(baseurl + "/"):
                        target = target[len(baseurl):]
                    else:
                        # An absolute link that omits the baseurl cannot resolve once deployed.
                        broken.append((page_url, href, "outside baseurl"))
                        continue
            else:
                # Relative: the browser resolves it against the *served* URL, so the baseurl
                # cancels out on both sides and `page_url` (already site-root-relative) is the
                # correct base. Stripping a baseurl here would be wrong — there is none to strip.
                target = urljoin(page_url, target)
            path = unquote(target)
            if not resolves(path):
                broken.append((page_url, href, "404"))

if broken:
    print(f"verify-docs-links: {len(broken)} broken internal link(s) across {pages} page(s):\n")
    for page_url, href, why in sorted(set(broken)):
        print(f"  {page_url}\n      -> {href}  [{why}]")
    print("\nA trailing-slash link needs a directory index. If these are all `/foo/` links whose")
    print("files are `foo.html`, the site is missing `permalink: pretty` in docs/_config.yml.")
    sys.exit(1)

print(f"verify-docs-links: OK — every internal link across {pages} page(s) resolves.")
PY
}

self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  # The exact #898 layout: the page is built as `features/spaces.html`, the link points at
  # `features/spaces/`. Live, this is a 404.
  mkdir -p "$tmp/broken/features"
  printf '<a href="/rift/features/spaces/">spaces</a>' > "$tmp/broken/index.html"
  printf 'spaces' > "$tmp/broken/features/spaces.html"

  # The same link under `permalink: pretty` — served by the directory index. Also covers the two
  # relative-link shapes the docs actually use: a relative link carries no baseurl, so resolving it
  # as though it did reports a false 404 (which is exactly what this checker did on first run).
  mkdir -p "$tmp/fixed/features/spaces" "$tmp/fixed/performance"
  printf '<a href="/rift/features/spaces/">abs</a><a href="performance/">rel</a>' > "$tmp/fixed/index.html"
  printf '<a href="../../performance/">up</a>' > "$tmp/fixed/features/spaces/index.html"
  printf 'perf' > "$tmp/fixed/performance/index.html"

  if check_site "$tmp/broken" "/rift" >/dev/null 2>&1; then
    echo "verify-docs-links --self-test: FAILED — checker passed the known-broken #898 layout." >&2
    return 1
  fi
  if ! check_site "$tmp/fixed" "/rift" >/dev/null 2>&1; then
    echo "verify-docs-links --self-test: FAILED — checker rejected a correctly-built site." >&2
    return 1
  fi
  echo "verify-docs-links --self-test: OK — flags the #898 layout, accepts the fixed one."
}

if [[ "${1:-}" == "--self-test" ]]; then
  self_test
else
  check_site "${1:-$repo_root/_site}" "$BASEURL"
fi
