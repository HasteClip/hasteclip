#!/usr/bin/env python3
"""Tests for download-stats.py: what counts, and how the days add up.

  download-stats-test.py
"""

import importlib.util
import json
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("stats", Path(__file__).with_name("download-stats.py"))
stats = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stats)

# v0.8.0's files and downloads on 2026-11-17.
V080 = {
    "assets.win.json": 1,
    "HasteClip-0.7.0-full.nupkg": 2,
    "HasteClip-0.8.0-delta.nupkg": 353,
    "HasteClip-0.8.0-full.nupkg": 9,
    "HasteClip-linux-x64.tar.gz": 35,
    "HasteClip-linux-x64.tar.gz.sha256": 16,
    "HasteClip-mac-arm64.dmg": 47,
    "HasteClip-mac-arm64.zip": 12,
    "HasteClip-mac-arm64.zip.sha256": 11,
    "HasteClip-web-engine-linux-x64-156.0.26.tar.xz": 3,
    "HasteClip-web-engine-linux-x64-156.0.26.tar.xz.sha256": 2,
    "HasteClip-win-Portable.zip": 5,
    "HasteClip-win-Setup.exe": 186,
    "RELEASES": 2,
    "releases.win.json": 33262,
}


def at(stamp):
    return datetime.fromisoformat(stamp).replace(tzinfo=timezone.utc)


def release(published, files, ids=None):
    return {"published": published, "files": files, "ids": ids or {name: f"{published}/{name}" for name in files}}


class WhatCounts(unittest.TestCase):
    def test_only_what_people_install_from(self):
        # Setup + Portable; the .dmg and the zip's one download beyond its
        # checksum's; the tarball's beyond its checksum's.
        self.assertEqual(stats.installs(V080), {"windows": 191, "macos": 48, "linux": 19})

    def test_more_checksums_than_downloads_isnt_below_zero(self):
        files = {"HasteClip-mac-arm64.zip": 1, "HasteClip-mac-arm64.zip.sha256": 2}
        self.assertEqual(stats.installs(files)["macos"], 0)

    def test_every_file_of_a_release_is_known(self):
        self.assertEqual([n for n in V080 if stats.platform_of(n) is None and not stats.left_out(n)], [])
        self.assertFalse(stats.left_out("HasteClip-linux-x64.AppImage"))


class Merge(unittest.TestCase):
    def test_a_deleted_release_keeps_its_downloads(self):
        before = stats.merge({}, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 10})})
        merged = stats.merge(before, {})
        self.assertTrue(merged["v1"]["gone"])
        self.assertEqual(stats.totals(merged)["windows"], 10)

    def test_a_file_uploaded_again_carries_what_it_had(self):
        before = stats.merge({}, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 10}, {"HasteClip-win-Setup.exe": 1})})
        # GitHub's count starts over: 4 since, under a new id.
        merged = stats.merge(before, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 4}, {"HasteClip-win-Setup.exe": 2})})
        self.assertEqual(merged["v1"]["files"]["HasteClip-win-Setup.exe"], 14)
        # And again: the 14 are carried, not just the first 10.
        again = stats.merge(merged, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 1}, {"HasteClip-win-Setup.exe": 3})})
        self.assertEqual(again["v1"]["files"]["HasteClip-win-Setup.exe"], 15)
        # Later counts add to it.
        later = stats.merge(again, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 6}, {"HasteClip-win-Setup.exe": 3})})
        self.assertEqual(later["v1"]["files"]["HasteClip-win-Setup.exe"], 20)

    def test_a_file_taken_out_keeps_its_downloads(self):
        files = {"HasteClip-win-Setup.exe": 10, "HasteClip-mac-arm64.dmg": 5}
        before = stats.merge({}, {"v1": release("2026-10-01T00:00:00Z", files)})
        merged = stats.merge(before, {"v1": release("2026-10-01T00:00:00Z", {"HasteClip-win-Setup.exe": 11})})
        self.assertEqual(stats.totals(merged), {"windows": 11, "macos": 5, "linux": 0})

    def test_update_checks_are_the_most_any_feed_grew(self):
        before = stats.merge({}, {
            "v1": release("2026-10-01T00:00:00Z", {"releases.win.json": 1000}),
            "v2": release("2026-10-05T00:00:00Z", {"releases.win.json": 400}),
        })
        merged = stats.merge(before, {
            "v1": release("2026-10-01T00:00:00Z", {"releases.win.json": 1300}),
            "v2": release("2026-10-05T00:00:00Z", {"releases.win.json": 700}),
            # New since the last run: it saw only some of the checks.
            "v3": release("2026-10-09T00:00:00Z", {"releases.win.json": 50}),
        })
        self.assertEqual(stats.checks_between(before, merged), 300)


class Days(unittest.TestCase):
    RELEASES = {
        "v1": release("2026-10-01T12:00:00Z", {"HasteClip-win-Setup.exe": 10, "HasteClip-linux-x64.tar.gz": 4, "releases.win.json": 900}),
        "v2": release("2026-10-03T00:00:00Z", {"HasteClip-win-Setup.exe": 30, "releases.win.json": 500}),
    }

    def test_backfill_spreads_each_release_over_its_days(self):
        rows = stats.backfill(stats.merge({}, self.RELEASES), at("2026-10-05T00:00:00"))
        self.assertEqual([r["date"] for r in rows], ["2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05"])
        # v1 was the newest Windows release for 1.5 days (a third of it on
        # the 1st), v2 for the 2 days after.
        self.assertEqual([r["windows"] for r in rows], [3, 10, 25, 40, 40])
        # No Linux in v2: v1's tarball was the newest till today.
        self.assertEqual(rows[-1]["linux"], 4)
        # 400 checks between v1 and v2, then 500 since.
        self.assertEqual(sum(r["windows_update_checks"] for r in rows), 900)
        self.assertEqual([r["estimated"] for r in rows], [1, 1, 1, 1, 0])

    def test_a_second_run_the_same_day_adds_to_the_day(self):
        rows = [{"date": "2026-10-05", "windows": 40, "macos": 0, "linux": 4, "windows_update_checks": 100, "estimated": 0}]
        stats.add_run(rows, {"windows": 42, "macos": 0, "linux": 4}, 20, at("2026-10-05T23:00:00"))
        self.assertEqual(rows, [{"date": "2026-10-05", "windows": 42, "macos": 0, "linux": 4, "windows_update_checks": 120, "estimated": 0}])

    def test_missed_days_get_an_even_share(self):
        rows = [{"date": "2026-10-05", "windows": 40, "macos": 0, "linux": 4, "windows_update_checks": 100, "estimated": 0}]
        stats.add_run(rows, {"windows": 49, "macos": 0, "linux": 3}, 301, at("2026-10-08T23:00:00"))
        self.assertEqual([r["windows"] for r in rows], [40, 43, 46, 49])
        # Never down: one Linux checksum fetched by hand.
        self.assertEqual([r["linux"] for r in rows], [4, 4, 4, 4])
        self.assertEqual([r["windows_update_checks"] for r in rows], [100, 100, 100, 101])
        self.assertEqual([r["estimated"] for r in rows], [0, 1, 1, 0])

    def test_two_runs_through_the_folder(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder)
            stats.update(folder, self.RELEASES, at("2026-10-05T12:00:00"))
            later = {tag: {**r, "files": dict(r["files"])} for tag, r in self.RELEASES.items()}
            later["v2"]["files"]["HasteClip-win-Setup.exe"] += 1500
            later["v1"]["files"]["releases.win.json"] += 70
            later["v2"]["files"]["releases.win.json"] += 70
            counts = stats.update(folder, later, at("2026-10-06T12:00:00"))
            self.assertEqual(counts, {"windows": 1540, "macos": 0, "linux": 4})
            rows = stats.read_rows(folder / "downloads.csv")
            self.assertEqual(rows[-1], {"date": "2026-10-06", "windows": 1540, "macos": 0, "linux": 4, "windows_update_checks": 70, "estimated": 0})
            badge = json.loads((folder / "downloads.json").read_text())
            self.assertEqual(badge["message"], "1.5k")
            for name in ("downloads.svg", "downloads-dark.svg", "snapshot.json", "README.md"):
                self.assertTrue((folder / name).is_file(), name)


class Drawing(unittest.TestCase):
    def test_counts_as_shields_writes_them(self):
        self.assertEqual([stats.short(n) for n in (950, 1000, 1577, 9960, 15500, 999_999, 1_500_000)], ["950", "1k", "1.6k", "10k", "16k", "1M", "1.5M"])

    def test_y_axis(self):
        self.assertEqual(stats.y_ticks(1577), [0, 500, 1000, 1500, 2000])
        self.assertEqual(stats.y_ticks(0), [0, 1])
        self.assertEqual(stats.y_label(25000, 50000), "25k")

    def test_x_axis(self):
        first = at("2026-10-16T00:00:00").date()
        self.assertEqual([label for _, label in stats.x_ticks(first, 33)], ["Oct 19", "Oct 26", "Nov 2", "Nov 9", "Nov 16"])
        self.assertEqual([label for _, label in stats.x_ticks(first, 3)], ["Oct 16", "Oct 17", "Oct 18"])
        self.assertEqual(stats.x_ticks(first, 800)[0][1], "Jan 2027")

    def test_one_day_and_years_of_days_draw(self):
        row = {"date": "2026-10-16", "windows": 1, "macos": 0, "linux": 0, "windows_update_checks": 0, "estimated": 0}
        self.assertIn("1 (Windows 1,", stats.draw([row], "light", at("2026-10-16T12:00:00")))
        rows = [{**row, "date": f"{2026 + i // 365}-01-01", "windows": i} for i in range(1200)]
        svg = stats.draw(rows, "dark", at("2029-04-15T12:00:00"))
        self.assertLess(len(svg), 60_000)


if __name__ == "__main__":
    unittest.main()
