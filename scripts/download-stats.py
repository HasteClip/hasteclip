#!/usr/bin/env python3
"""HasteClip's downloads: the numbers, graph and badge in the README.

  download-stats.py <folder>

GitHub counts every download of every file in a release, and that's the only
record there is: the website's buttons and the README link straight to the
releases. Most of those downloads aren't people getting HasteClip, though. On
Windows, every update check (at launch, then every half hour) fetches each
recent release's releases.win.json, and an update fetches a .nupkg. On macOS
and Linux, an update fetches the .zip or the tarball, after its .sha256. So
only the files people install from are counted:

  Windows  HasteClip-win-Setup.exe and HasteClip-win-Portable.zip.
  macOS    HasteClip-mac-arm64.dmg, and the .zip's downloads beyond its
           .sha256's: people who took the zip themselves.
  Linux    HasteClip-linux-x64.tar.gz's downloads beyond its .sha256's.
           Updates download the same tarball, but always fetch its checksum
           first. Anyone who checks the checksum by hand is taken off too, so
           it's a little low.

The workflow (.github/workflows/download-stats.yml) runs this once a day on
a checkout of the stats branch. It adds the day to downloads.csv, keeps every
file's count in snapshot.json, and redraws the graph (downloads.svg,
downloads-dark.svg) and the badge (downloads.json, which shields.io's
endpoint badge reads). The first run fills in the days since the first
release from the releases' counts. GITHUB_TOKEN is used if it's set.
"""

import csv
import json
import math
import os
import sys
import urllib.request
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

REPO = "HasteClip/hasteclip"
PLATFORMS = {"windows": "Windows", "macos": "macOS", "linux": "Linux"}
COLUMNS = ["date", *PLATFORMS, "windows_update_checks", "estimated"]
# A Windows update check fetches the feed of each of this many newest
# releases (Velopack's GithubSource).
FEED_RELEASES = 10
BADGE_COLOR = "3dd68c"  # HasteClip's green
MONTHS = "Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split()


# What counts --------------------------------------------------------------

def platform_of(name):
    """The platform a release file installs HasteClip on, if people install from it."""
    if name.endswith(("-Setup.exe", "-Portable.zip")):
        return "windows"
    if name.endswith(".dmg") or name.startswith("HasteClip-mac-") and name.endswith(".zip"):
        return "macos"
    if name.startswith("HasteClip-linux-") and name.endswith(".tar.gz"):
        return "linux"
    return None


def installs(files):
    """Installs per platform, from one release's {file: downloads}."""
    n = dict.fromkeys(PLATFORMS, 0)
    for name, count in files.items():
        platform = platform_of(name)
        if platform is None:
            continue
        # Updates fetch the macOS .zip and the Linux tarball, always after the
        # file's .sha256: only the downloads beyond the checksum's are people's.
        if platform != "windows" and name.endswith((".zip", ".tar.gz")):
            count = max(0, count - files.get(name + ".sha256", 0))
        n[platform] += count
    return n


def is_feed(name):
    """What a Windows update check fetches from each recent release."""
    return name.startswith("releases.") and name.endswith(".json")


def left_out(name):
    """A file that the app or the release workflow fetches, not people."""
    return (
        is_feed(name)
        or name == "RELEASES"
        or name.startswith("assets.") and name.endswith(".json")
        or name.endswith((".nupkg", ".sha256"))
        or name.startswith("HasteClip-web-engine-")
    )


def fetch_releases():
    """Every published release: {tag: {published, files: {name: downloads}, ids: {name: id}}}."""
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "hasteclip-download-stats"}
    if os.environ.get("GITHUB_TOKEN"):
        headers["Authorization"] = "Bearer " + os.environ["GITHUB_TOKEN"]
    releases = {}
    for page in range(1, 100):
        url = f"https://api.github.com/repos/{REPO}/releases?per_page=100&page={page}"
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as response:
            batch = json.load(response)
        for release in batch:
            # A token that can push sees drafts too, and they have no date.
            if release["draft"] or not release["published_at"]:
                continue
            releases[release["tag_name"]] = {
                "published": release["published_at"],
                "files": {a["name"]: a["download_count"] for a in release["assets"]},
                "ids": {a["name"]: a["id"] for a in release["assets"]},
            }
        if len(batch) < 100:
            break
    return releases


def merge(before, now):
    """Every release's counts, kept from going down where GitHub's do. A
    deleted release keeps its last counts, and so does a file taken out of a
    release. A file uploaded again starts over at 0 on GitHub, so it carries
    the downloads it had."""
    merged = {tag: {**old, "gone": True} for tag, old in before.items() if tag not in now}
    for tag, release in now.items():
        old = before.get(tag, {})
        old_files, old_ids = old.get("files", {}), old.get("ids", {})
        carried = dict(old.get("carried", {}))
        files, ids = dict(old_files), dict(old_ids)
        for name, count in release["files"].items():
            if name in old_ids and old_ids[name] != release["ids"][name]:
                carried[name] = old_files[name]
            files[name] = count + carried.get(name, 0)
            ids[name] = release["ids"][name]
        merged[tag] = {"published": release["published"], "files": files, "ids": ids}
        if carried:
            merged[tag]["carried"] = carried
    return merged


def totals(releases):
    """Installs so far per platform, over every release."""
    n = dict.fromkeys(PLATFORMS, 0)
    for release in releases.values():
        for platform, count in installs(release["files"]).items():
            n[platform] += count
    return n


def checks_between(before, now):
    """Windows update checks between two runs. Every check fetches the feed of
    each recent release, so it's the most that any one feed grew."""
    grew = {}
    for tag, release in now.items():
        if release.get("gone") or tag not in before:
            continue
        for name, count in release["files"].items():
            if is_feed(name):
                grew[name] = max(grew.get(name, 0), count - before[tag]["files"].get(name, 0))
    return sum(grew.values())


# Day by day ----------------------------------------------------------------

def at(stamp):
    return datetime.fromisoformat(stamp.replace("Z", "+00:00"))


def midnight(day):
    return datetime(day.year, day.month, day.day, tzinfo=timezone.utc)


def spread(amount, start, end, days, into):
    """Adds `amount` over the days from `start` to `end`, each its share of the time."""
    i = max(0, (start.date() - days[0]).days)
    if end <= start:
        into[min(i, len(days) - 1)] += amount
        return
    length = (end - start).total_seconds()
    while start < end and i < len(days):
        piece = min(end, midnight(days[i] + timedelta(1)))
        into[i] += amount * (piece - start).total_seconds() / length
        start, i = piece, i + 1


def backfill(releases, now):
    """The days from the first release to today, from before there were daily
    counts. Every download link points at the newest release, so each
    release's installs on a platform are spread over the days until the next
    release for that platform. Each release's feed counts every update check
    since it came out, so the checks between two releases are the difference
    between their feeds' counts, spread the same way."""
    order = sorted(releases.values(), key=lambda r: r["published"])
    first = at(order[0]["published"]).date()
    days = [first + timedelta(i) for i in range((now.date() - first).days + 1)]
    added = {platform: [0.0] * len(days) for platform in PLATFORMS}
    for platform in PLATFORMS:
        having = [r for r in order if any(platform_of(name) == platform for name in r["files"])]
        for i, release in enumerate(having):
            end = at(having[i + 1]["published"]) if i + 1 < len(having) else now
            spread(installs(release["files"])[platform], at(release["published"]), end, days, added[platform])
    checks = [0.0] * len(days)
    feeds = [(at(r["published"]), sum(c for name, c in r["files"].items() if is_feed(name))) for r in order[-FEED_RELEASES:]]
    feeds = [(when, count) for when, count in feeds if count]
    if feeds:
        so_far = [(when, feeds[0][1] - count) for when, count in feeds] + [(now, feeds[0][1])]
        for (t0, c0), (t1, c1) in zip(so_far, so_far[1:]):
            spread(max(0, c1 - c0), t0, t1, days, checks)
    rows, running = [], dict.fromkeys(PLATFORMS, 0.0)
    for i, day in enumerate(days):
        for platform in PLATFORMS:
            running[platform] += added[platform][i]
        rows.append({
            "date": day.isoformat(),
            **{platform: round(running[platform]) for platform in PLATFORMS},
            "windows_update_checks": round(checks[i]),
            "estimated": 1,
        })
    # Today's are counted, not spread.
    rows[-1].update(totals(releases), estimated=0)
    return rows


def add_run(rows, counts, checks, now):
    """Adds a run to the rows: today's installs so far, and the update checks
    since the last run. Days the workflow missed in between get an even share."""
    last = rows[-1]
    # Never down: a Linux checksum someone fetched by hand takes one off
    # until the next install.
    counts = {platform: max(counts[platform], last[platform]) for platform in PLATFORMS}
    last_day = date.fromisoformat(last["date"])
    gap = (now.date() - last_day).days
    if gap <= 0:
        last.update(counts)
        last["windows_update_checks"] += checks
        return
    for i in range(1, gap + 1):
        rows.append({
            "date": (last_day + timedelta(i)).isoformat(),
            **{platform: last[platform] + (counts[platform] - last[platform]) * i // gap for platform in PLATFORMS},
            "windows_update_checks": checks * i // gap - checks * (i - 1) // gap,
            "estimated": int(i < gap),
        })


# The graph and badge -------------------------------------------------------

W, H = 800, 400
LEFT, RIGHT, TOP, BOTTOM = 56, 776, 92, 356
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
# Series colors checked for colorblind separation as a set, on GitHub's
# light (#ffffff) and dark (#0d1117) pages; the rest are GitHub's own grays.
THEMES = {
    "light": {
        "windows": "#2a78d6", "macos": "#eb6834", "linux": "#1baf7a",
        "ink": "#1f2328", "ink2": "#59636e", "muted": "#6e7781",
        "grid": "#e1e6eb", "axis": "#c8d1da", "estimated": "#f6f8fa",
    },
    "dark": {
        "windows": "#3987e5", "macos": "#d95926", "linux": "#199e70",
        "ink": "#f0f6fc", "ink2": "#9198a1", "muted": "#7d8590",
        "grid": "#262c35", "axis": "#3d444d", "estimated": "#151b23",
    },
}


def short(n):
    """A count as shields.io writes one: 950, 1.6k, 16k, 1.6M."""
    for power, suffix in ((6, "M"), (3, "k")):
        scaled = n / 10**power
        if scaled >= 1:
            if scaled < 9.95:
                return f"{scaled:.1f}".removesuffix(".0") + suffix
            if round(scaled) < 1000 or suffix == "M":
                return f"{round(scaled)}{suffix}"
            return "1M"
    return str(n)


def y_ticks(top):
    """0 and round steps up to `top` or just past it: five lines at most."""
    for power in range(15):
        for m in (1, 2, 5) if power == 0 else (1, 2, 2.5, 5):
            step = int(m * 10**power)
            if step * 4 >= top:
                return [step * i for i in range(max(1, math.ceil(top / step)) + 1)]
    raise ValueError(top)


def y_label(v, top):
    if top >= 10**7:
        return f"{v / 10**6:g}M"
    if top >= 10**4:
        return f"{v / 1000:g}k"
    return f"{v:,}"


def x_ticks(first, days):
    """The dates to label along the bottom, eight at most: days, Mondays or
    the first of a month."""
    end = first + timedelta(days)
    for every in (1, 2, 7, 14):
        start = first if every < 7 else first + timedelta((7 - first.weekday()) % 7)
        ticks = [start + timedelta(i) for i in range(0, (end - start).days, every)]
        if len(ticks) <= 8:
            return [(d, f"{MONTHS[d.month - 1]} {d.day}") for d in ticks]
    months, d = [], date(first.year, first.month, 1)
    while d < end:
        if d >= first:
            months.append(d)
        d = date(d.year + d.month // 12, d.month % 12 + 1, 1)
    for every in (1, 2, 3, 6, 12, 24, 60, 120):
        ticks = [d for d in months if (d.year * 12 + d.month - 1) % every == 0]
        if len(ticks) <= 8:
            return [(d, f"{d.year}" if every >= 12 else f"{MONTHS[d.month - 1]} {d.year}") for d in ticks]
    return []


def path(points):
    return "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in points)


def height_at(points, x):
    """Where a line through `points` (left to right) is at `x`."""
    for (x0, y0), (x1, y1) in zip(points, points[1:]):
        if x0 <= x <= x1:
            return y0 if x1 == x0 else y0 + (y1 - y0) * (x - x0) / (x1 - x0)
    return points[-1][1]


def draw(rows, theme, now):
    """The graph: installs so far, stacked by platform."""
    c = THEMES[theme]
    days, last = len(rows), rows[-1]
    total = sum(last[platform] for platform in PLATFORMS)
    ticks = y_ticks(total)
    top = ticks[-1]

    def x(day):
        return LEFT + (RIGHT - LEFT) * day / days

    def y(v):
        return BOTTOM - (BOTTOM - TOP) * v / top

    # A point at the end of each day (at most about 400 of them), from 0
    # before the first.
    every = max(1, math.ceil(days / 400))
    shown = list(range(every - 1, days, every))
    if shown[-1] != days - 1:
        shown.append(days - 1)
    xs = [x(0)] + [x(i + 1) for i in shown]
    bands, below = {}, [0] * (len(shown) + 1)
    for platform in PLATFORMS:
        above = [b + v for b, v in zip(below, [0] + [rows[i][platform] for i in shown])]
        bands[platform] = (below, above)
        below = above

    updated = f"{MONTHS[now.month - 1]} {now.day}, {now.year}"
    split = ", ".join(f"{name} {last[p]:,}" for p, name in PLATFORMS.items())
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="{FONT}" role="img">',
        f"<title>HasteClip downloads: {total:,} ({split}). Updated {updated}.</title>",
    ]

    # The days before daily counts began, spread from release dates.
    counted = next((i for i, row in enumerate(rows) if not row["estimated"]), days - 1)
    if counted > 0:
        edge = x(counted + 1)
        out.append(f'<rect x="{LEFT}" y="{TOP}" width="{edge - LEFT:.1f}" height="{BOTTOM - TOP}" fill="{c["estimated"]}"/>')
        if edge - LEFT >= 90:
            out.append(f'<text x="{LEFT + 8}" y="{TOP + 18}" font-size="11" fill="{c["muted"]}">Estimated</text>')

    for v in ticks[1:]:
        line_y = round(y(v)) + 0.5
        out.append(f'<line x1="{LEFT}" x2="{RIGHT}" y1="{line_y}" y2="{line_y}" stroke="{c["grid"]}" shape-rendering="crispEdges"/>')
    for v in ticks:
        out.append(
            f'<text x="{LEFT - 8}" y="{round(y(v)) + 4}" text-anchor="end" font-size="12" fill="{c["muted"]}"'
            f' style="font-variant-numeric:tabular-nums">{y_label(v, top)}</text>'
        )

    edges = {platform: [list(zip(xs, map(y, lo))), list(zip(xs, map(y, hi)))] for platform, (lo, hi) in bands.items()}
    for platform, (lower, upper) in edges.items():
        out.append(f'<path d="{path(upper + lower[::-1])}Z" fill="{c[platform]}" fill-opacity="0.12"/>')
    for platform, (lower, upper) in edges.items():
        out.append(f'<path d="{path(upper)}" fill="none" stroke="{c[platform]}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>')
    base_y = round(y(0)) + 0.5
    out.append(f'<line x1="{LEFT}" x2="{RIGHT}" y1="{base_y}" y2="{base_y}" stroke="{c["axis"]}" shape-rendering="crispEdges"/>')

    first = date.fromisoformat(rows[0]["date"])
    for d, label in x_ticks(first, days):
        out.append(f'<text x="{x((d - first).days):.1f}" y="{BOTTOM + 24}" text-anchor="middle" font-size="12" fill="{c["muted"]}">{label}</text>')

    # Each band's name at its right end, where the band is tall enough for it.
    for platform, name in PLATFORMS.items():
        lower, upper = edges[platform]
        under = RIGHT - 8 - 3.5 * len(name)  # about the middle of the name
        low, high = height_at(lower, under), height_at(upper, under)
        if low - high >= 16:
            out.append(f'<text x="{RIGHT - 8}" y="{(low + high) / 2 + 4:.1f}" text-anchor="end" font-size="12" fill="{c["ink"]}">{name}</text>')

    out.append(
        f'<text x="{LEFT}" y="44"><tspan font-size="30" font-weight="600" fill="{c["ink"]}">{total:,}</tspan>'
        f'<tspan dx="8" font-size="15" fill="{c["ink2"]}">downloads</tspan></text>'
    )
    out.append(f'<text x="{RIGHT}" y="44" text-anchor="end" font-size="12" fill="{c["muted"]}">Updated {updated}</text>')
    legend = "".join(
        f'<tspan dx="{18 if i else 0}" dy="1" font-size="17" fill="{c[p]}">&#9679;</tspan>'
        f'<tspan dx="5" dy="-1" fill="{c["ink2"]}">{name}</tspan>'
        f'<tspan dx="5" font-weight="600" fill="{c["ink"]}">{last[p]:,}</tspan>'
        for i, (p, name) in enumerate(PLATFORMS.items())
    )
    out.append(f'<text x="{LEFT}" y="72" font-size="13">{legend}</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


STATS_README = """\
# Download stats

The numbers behind the downloads graph and badge in [HasteClip's README](https://github.com/HasteClip/hasteclip#download-history).
[`download-stats.yml`](https://github.com/HasteClip/hasteclip/blob/main/.github/workflows/download-stats.yml)
adds each day's once a day, with [`scripts/download-stats.py`](https://github.com/HasteClip/hasteclip/blob/main/scripts/download-stats.py).
Nothing here is edited by hand.

## What counts as a download

GitHub counts every download of every file in a release, and most of them aren't
people getting HasteClip: the app's update checks and updates download files too.
Only the files people install from are counted:

| System | Counted | Not counted |
|---|---|---|
| Windows | `HasteClip-win-Setup.exe`, `HasteClip-win-Portable.zip` | update checks (`releases.win.json`), updates (`.nupkg`) |
| macOS | `HasteClip-mac-arm64.dmg`, and the `.zip`'s downloads beyond its `.sha256`'s | updates (the `.zip` and its `.sha256`) |
| Linux | `HasteClip-linux-x64.tar.gz`'s downloads beyond its `.sha256`'s | updates (the same tarball, after its `.sha256`) |

Linux's is a little low: updates download the same tarball people do, but always
fetch its checksum first, so the checksum's downloads are taken off. Anyone who
checks the checksum by hand is taken off too.

## The files

- `downloads.csv`: a row a day. `windows`, `macos` and `linux` are the installs
  so far at the end of that day (today's at the last run).
  `windows_update_checks` is the update checks that day: about how many copies
  of HasteClip are running on Windows, times how often each checks. `estimated`
  is 1 where the day's numbers are spread rather than counted. That's the days
  before daily counting began, spread over the days each release was the
  newest, and any days the workflow missed.
- `snapshot.json`: every release's download count per file at the last run.
  The branch's history has every day's.
- `downloads.svg`, `downloads-dark.svg`: the graph, for light and dark pages.
- `downloads.json`: the badge, read by [shields.io's endpoint badge](https://shields.io/badges/endpoint-badge).
"""


# Files ---------------------------------------------------------------------

def write(path, text):
    path.write_text(text, encoding="utf-8", newline="\n")


def read_rows(path):
    if not path.exists():
        return []
    with path.open(newline="") as f:
        return [{k: v if k == "date" else int(v) for k, v in row.items()} for row in csv.DictReader(f)]


def write_rows(path, rows):
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(f, COLUMNS, lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def update(folder, releases, now):
    """Counts this run into `folder` and redraws the graph and badge. Returns
    the installs so far per platform."""
    snapshot, table = folder / "snapshot.json", folder / "downloads.csv"
    before = json.loads(snapshot.read_text(encoding="utf-8"))["releases"] if snapshot.exists() else {}
    rows = read_rows(table)
    merged = merge(before, releases)
    if before and rows:
        add_run(rows, totals(merged), checks_between(before, merged), now)
    else:
        rows = backfill(merged, now)
    newest_first = dict(sorted(merged.items(), key=lambda kv: kv[1]["published"], reverse=True))
    write(snapshot, json.dumps({"time": now.strftime("%Y-%m-%dT%H:%M:%SZ"), "releases": newest_first}, indent=1) + "\n")
    write_rows(table, rows)
    last = rows[-1]
    total = sum(last[platform] for platform in PLATFORMS)
    badge = {"schemaVersion": 1, "label": "downloads", "message": short(total), "color": BADGE_COLOR}
    write(folder / "downloads.json", json.dumps(badge) + "\n")
    write(folder / "downloads.svg", draw(rows, "light", now))
    write(folder / "downloads-dark.svg", draw(rows, "dark", now))
    write(folder / "README.md", STATS_README)
    return {platform: last[platform] for platform in PLATFORMS}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.split("\n\n")[1])
    folder = Path(sys.argv[1])
    folder.mkdir(parents=True, exist_ok=True)
    releases = fetch_releases()
    if not releases:
        sys.exit("No releases yet.")
    for tag, release in releases.items():
        for name in release["files"]:
            if platform_of(name) is None and not left_out(name):
                # A new kind of file: it needs a rule, counted or left out.
                print(f"::warning::{tag} has {name}, which download-stats.py doesn't know: is it counted?")
    counts = update(folder, releases, datetime.now(timezone.utc))
    print(", ".join(f"{name} {counts[p]:,}" for p, name in PLATFORMS.items()), f"= {sum(counts.values()):,} downloads")


if __name__ == "__main__":
    main()
