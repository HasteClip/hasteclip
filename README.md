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
