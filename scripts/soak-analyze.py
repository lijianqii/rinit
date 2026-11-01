#!/usr/bin/env python3
"""Analyze one long-soak result directory.

The guest CSV is authoritative for the resource curve; the serial log supplies
the final SOAK_DONE/FEATURE_DONE verdict. Feature-active samples are excluded
from the RSS/fd trend, but still checked for control and service health.
"""
import argparse
import csv
import json
import os
import re
import statistics
import sys


def parse_args():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--dir", default="build/soak-charts", help="log directory")
    p.add_argument("--tag", required=True, help="soak-qemu tag")
    p.add_argument("--json", help="write the summary here")
    p.add_argument("--require-feature", action="store_true",
                   help="fail if the feature-results CSV is missing")
    return p.parse_args()


def read_run_info(path):
    info = {}
    if not os.path.exists(path):
        return info
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            key, _, value = line.partition(" ")
            info[key] = value.strip()
    return info


def read_serial(path):
    if not os.path.exists(path):
        return ""
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def read_rows(path):
    with open(path, newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        rows = [r for r in reader if r.get("elapsed", "").isdigit()]
    return rows


def read_feature_rows(path):
    if not path or not os.path.exists(path):
        return []
    with open(path, newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        return list(reader)


def median(values):
    return statistics.median(values) if values else 0.0


def slope_kb_per_hour(rows):
    if len(rows) < 2:
        return 0.0
    xs = [int(r["elapsed"]) / 3600.0 for r in rows]
    ys = [float(r["rss_kb"]) for r in rows]
    mx = sum(xs) / len(xs)
    my = sum(ys) / len(ys)
    denom = sum((x - mx) ** 2 for x in xs)
    if denom == 0:
        return 0.0
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / denom


def main():
    args = parse_args()
    prefix = os.path.join(args.dir, f"soak-qemu-{args.tag}")
    csv_path = prefix + "-guest.csv"
    serial_path = prefix + ".log"
    run_path = prefix + "-run.txt"
    feature_path = prefix + "-features.csv"

    reasons = []
    info = read_run_info(run_path)
    serial = read_serial(serial_path)
    rows = read_rows(csv_path) if os.path.exists(csv_path) else []
    feature_rows = read_feature_rows(feature_path)
    feature_enabled = bool(feature_rows)
    fd_limit = 4 if feature_enabled else 2
    drift_limit = 512 if feature_enabled else 128
    slope_limit = 16.0 if feature_enabled else 1.0

    duration = 0
    interval = 0
    if info.get("duration"):
        match = re.search(r"(\d+)s\s+interval\s+(\d+)s", info["duration"])
        if match:
            duration = int(match.group(1))
            interval = int(match.group(2))
    expected = duration // interval if duration and interval else 0

    if not rows:
        reasons.append("guest CSV is missing or empty")
    if expected and len(rows) < expected:
        reasons.append(f"only {len(rows)} of {expected} samples")

    if "SOAK_DONE status=OK" not in serial:
        reasons.append("serial log does not contain SOAK_DONE status=OK")

    # Feature probes are deliberately excluded from the resource trend.
    warmup = 7200 if duration >= 4 * 3600 else 0
    stable = []
    for r in rows:
        if r.get("feature_active", "0") == "1":
            continue
        if int(r["elapsed"]) < warmup:
            continue
        stable.append(r)
    if len(stable) < 2:
        stable = rows

    control_fail = sum(int(r.get("control_fail", "0") or 0) for r in rows)
    if control_fail:
        reasons.append(f"{control_fail} control failures")

    fds = [int(r["fds"]) for r in rows if r.get("fds", "").isdigit()]
    fd_span = (max(fds) - min(fds)) if fds else 0
    if fd_span > fd_limit:
        reasons.append(f"fd span {fd_span} exceeds {fd_limit}")

    threads = [int(r["threads"]) for r in rows if r.get("threads", "").isdigit()]
    if threads and max(threads) > 2:
        reasons.append(f"threads peak {max(threads)} exceeds 2")

    zombies = [int(r["zombies"]) for r in rows if r.get("zombies", "").isdigit()]
    zombie_max = max(zombies) if zombies else 0
    if zombie_max:
        reasons.append(f"zombies observed: {zombie_max}")

    disk = [int(r["disk_free_kb"]) for r in rows
            if r.get("disk_free_kb", "").isdigit()]
    disk_min = min(disk) if disk else None
    if disk_min is not None and disk_min < 8192:
        reasons.append(f"guest disk free {disk_min} kB below 8192 kB")

    bad_health = 0
    for r in rows:
        if r.get("network") != "active" or r.get("sshd") != "active":
            bad_health += 1
        if r.get("device_target") not in (None, "", "active"):
            bad_health += 1
        if not r.get("ip") or not r.get("route"):
            bad_health += 1
        if r.get("udhcpc", "0").isdigit() and int(r["udhcpc"]) < 1:
            bad_health += 1
        if r.get("dropbear", "0").isdigit() and int(r["dropbear"]) < 1:
            bad_health += 1
    if bad_health:
        reasons.append(f"{bad_health} unhealthy device/network samples")

    rss = [float(r["rss_kb"]) for r in stable if r.get("rss_kb")]
    rss_first = median(rss[: len(rss) // 2])
    rss_second = median(rss[len(rss) // 2:])
    rss_drift = rss_second - rss_first
    rss_slope = slope_kb_per_hour(stable)
    # One-hour preflights are for wiring and functional probes, not leak
    # acceptance: a feature probe can move RSS once and a half-vs-half median
    # then reads as growth. Apply the leak thresholds only to runs long enough
    # for a leak to dominate allocator/cache noise.
    if rss and max(rss) > 4096:
        reasons.append(f"RSS peak {max(rss):.0f} kB exceeds the 4 MB budget")
    if duration >= 6 * 3600:
        if rss_drift > drift_limit:
            reasons.append(
                f"RSS median drift {rss_drift:.0f} kB exceeds {drift_limit} kB"
            )
        if rss_slope > slope_limit:
            reasons.append(
                f"RSS slope {rss_slope:.2f} kB/h exceeds {slope_limit:.1f} kB/h"
            )

    feature_failures = []
    for r in feature_rows:
        if r.get("result") != "pass":
            feature_failures.append(
                f"{r.get('scenario')}: {r.get('result')} "
                f"(expected_fail={r.get('expected_fail')})"
            )
    if args.require_feature and not feature_rows:
        reasons.append("feature-results CSV is missing")
    if "FEATURE_DONE status=OK" not in serial and feature_rows:
        reasons.append("serial log does not contain FEATURE_DONE status=OK")
    if feature_failures:
        reasons.append(f"{len(feature_failures)} feature probes failed")

    summary = {
        "tag": args.tag,
        "commit": info.get("commit"),
        "arch": info.get("arch"),
        "duration_s": duration,
        "interval_s": interval,
        "expected_samples": expected,
        "samples": len(rows),
        "stable_samples": len(stable),
        "rss_first_half_kb": round(rss_first, 1),
        "rss_second_half_kb": round(rss_second, 1),
        "rss_drift_kb": round(rss_drift, 1),
        "rss_slope_kb_per_hour": round(rss_slope, 3),
        "fds_min": min(fds) if fds else None,
        "fds_max": max(fds) if fds else None,
        "threads_max": max(threads) if threads else None,
        "zombies_max": zombie_max,
        "control_fail": control_fail,
        "disk_free_min_kb": disk_min,
        "bad_health_samples": bad_health,
        "feature_enabled": feature_enabled,
        "feature_runs": len(feature_rows),
        "feature_failures": feature_failures,
        "limits": {
            "rss_peak_kb": 4096,
            "rss_drift_kb": drift_limit,
            "rss_slope_kb_per_hour": slope_limit,
            "fd_span": fd_limit,
        },
        "reasons": reasons,
        "result": "OK" if not reasons else "FAIL",
    }

    print(json.dumps(summary, indent=2, ensure_ascii=False))
    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(summary, f, indent=2, ensure_ascii=False)
            f.write("\n")
    return 0 if not reasons else 1


if __name__ == "__main__":
    sys.exit(main())
