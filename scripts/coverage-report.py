#!/usr/bin/env python3
"""Render a cargo-llvm-cov JSON export into a Markdown coverage report.

Pure data transform (JSON in, Markdown out) — no cargo invocation — so it can
run in CI (to post a sticky PR comment / job summary) and locally to preview
exactly what CI will post. Produce the JSON it consumes with:

    scripts/coverage.sh                 # writes target/llvm-cov/coverage.json
    # or directly:
    cargo llvm-cov report --json --output-path target/llvm-cov/coverage.json

Usage:
    scripts/coverage-report.py [JSON] [--root DIR] [--floor N] [--target N]
                                      [--commit SHA] [--max-gaps N]

JSON defaults to target/llvm-cov/coverage.json. --root (default: cwd) is the
workspace root used to turn absolute filenames into crate-relative paths.
"""

import argparse
import json
import os
import sys

# Coverage tiers, aligned with the gate floor and the ratchet target.
GREEN, YELLOW = 85.0, 75.0


def emoji(pct: float) -> str:
    if pct >= GREEN:
        return "🟢"
    if pct >= YELLOW:
        return "🟡"
    return "🔴"


def bar(pct: float, width: int = 20) -> str:
    filled = int(round(pct / 100 * width))
    filled = max(0, min(width, filled))
    return "█" * filled + "░" * (width - filled)


def n(x: int) -> str:
    return f"{x:,}"


def crate_of(rel: str) -> str:
    parts = rel.split(os.sep)
    if parts[0] == "crates" and len(parts) >= 2:
        return parts[1]
    return parts[0]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("json", nargs="?", default="target/llvm-cov/coverage.json")
    ap.add_argument("--root", default=os.getcwd())
    ap.add_argument("--floor", type=float, default=75.0)
    ap.add_argument("--target", type=float, default=85.0)
    ap.add_argument("--commit", default=os.environ.get("GITHUB_SHA", ""))
    ap.add_argument("--max-gaps", type=int, default=12)
    ap.add_argument("--min-gap-lines", type=int, default=30)
    args = ap.parse_args()

    with open(args.json) as f:
        doc = json.load(f)

    data = doc["data"][0]
    totals = data["totals"]

    # Aggregate per-crate line counts and collect per-file rows for the gap list.
    crates: dict[str, dict[str, int]] = {}
    files = []
    for entry in data["files"]:
        rel = os.path.relpath(entry["filename"], args.root)
        if rel.startswith(".."):
            continue  # outside the workspace (e.g. registry deps)
        ln = entry["summary"]["lines"]
        count, covered = ln["count"], ln["covered"]
        if count == 0:
            continue
        crate = crate_of(rel)
        agg = crates.setdefault(crate, {"count": 0, "covered": 0})
        agg["count"] += count
        agg["covered"] += covered
        files.append({
            "rel": rel,
            "count": count,
            "covered": covered,
            "missed": count - covered,
            "pct": ln["percent"],
        })

    def pct(covered: int, count: int) -> float:
        return 100.0 * covered / count if count else 0.0

    out = []
    line_pct = totals["lines"]["percent"]
    out.append("## 📊 Coverage Report")
    out.append("")
    out.append(
        f"### {emoji(line_pct)} **{line_pct:.1f}%** line coverage "
        f"&nbsp;·&nbsp; floor **{args.floor:.0f}%** &nbsp;·&nbsp; target **{args.target:.0f}%**"
    )
    out.append("")
    out.append(f"`{bar(line_pct)}` **{line_pct:.1f}%**")
    out.append("")

    out.append("| Metric | Coverage | Covered / Total |")
    out.append("|---|---|---|")
    for label, key in (("Lines", "lines"), ("Functions", "functions"), ("Regions", "regions")):
        m = totals[key]
        out.append(f"| {label} | {m['percent']:.1f}% | {n(m['covered'])} / {n(m['count'])} |")
    out.append("")

    # Collapsed by default — keeps the comment compact while still one click away.
    out.append(f"<details><summary><b>By crate</b> ({len(crates)})</summary>")
    out.append("")
    out.append("| Crate | Coverage | Lines |")
    out.append("|---|---|---|")
    rows = []
    for crate, agg in crates.items():
        p = pct(agg["covered"], agg["count"])
        rows.append((p, crate, agg))
    # Lowest coverage first — the crates that most need attention float to the top.
    for p, crate, agg in sorted(rows, key=lambda r: (r[0], -r[2]["count"])):
        out.append(
            f"| `{crate}` | {emoji(p)} `{bar(p, 12)}` {p:.0f}% "
            f"| {n(agg['covered'])} / {n(agg['count'])} |"
        )
    out.append("")
    out.append("</details>")
    out.append("")

    gaps = [
        f for f in files
        if f["count"] >= args.min_gap_lines and f["pct"] < args.target
    ]
    gaps.sort(key=lambda f: f["missed"], reverse=True)
    gaps = gaps[: args.max_gaps]
    if gaps:
        out.append("### 🔍 Notable gaps")
        out.append("")
        out.append(
            f"Files with the most uncovered lines "
            f"(≥ {args.min_gap_lines} lines, below the {args.target:.0f}% target):"
        )
        out.append("")
        out.append("| File | Coverage | Missed | Lines |")
        out.append("|---|---|---|---|")
        for f in gaps:
            out.append(
                f"| `{f['rel']}` | {emoji(f['pct'])} {f['pct']:.0f}% "
                f"| {n(f['missed'])} | {n(f['covered'])} / {n(f['count'])} |"
            )
        out.append("")

    commit = f" · commit `{args.commit[:7]}`" if args.commit else ""
    out.append(
        f"<sub>Generated by <code>scripts/coverage-report.py</code> from "
        f"cargo-llvm-cov · gate: <code>scripts/coverage.sh</code>{commit}</sub>"
    )

    sys.stdout.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
