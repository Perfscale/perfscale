#!/usr/bin/env python3
"""Compare two bench-results.json files (previous run vs current run).

Usage: bench_compare.py <old.json> <new.json> [threshold-pct]

Prints a markdown delta table. Rows whose change exceeds the threshold
(default 15%) in the bad direction are flagged. Exit code is always 0 —
benchmarks on shared CI runners are noisy, so regressions warn, not fail.

Tolerates structural drift: suites or labels present on only one side are
skipped silently, so old artifacts stay comparable after suite changes.
"""

import json
import sys

DEFAULT_THRESHOLD = 15.0

# (results key, metric key, higher_is_better)
ROW_METRICS = [
    ("throughput", "rps", True),
    ("throughput", "p95_ms", False),
    ("throughput", "rss_mib", False),
    ("scaling", "rps", True),
    ("saturation", "rps", True),
    ("yaml", "rps", True),
    ("tls", "rps", True),
    ("library", "rps", True),
    ("grpc", "rps", True),
    ("graphql", "rps", True),
    ("db", "rps", True),
    ("pubsub", "rps", True),
    ("pubsub", "p95_ms", False),
    ("startup", "overhead_vs_ideal_ms", False),
    # SPECjbb2015: jOPS (Java operations/s), not HTTP RPS. Compared only
    # between rows of the same run mode — max-jOPS across modes (e.g.
    # COMPOSITE vs MULTI) are not the same quantity.
    ("specjbb", "max_jops", True),
    ("specjbb", "critical_jops", True),
    # Renaissance: benchmark operation throughput (measured ops/s, NOT HTTP
    # RPS) and per-operation time. Rows are labeled by benchmark, so only
    # the same benchmark is ever compared.
    ("renaissance", "ops_per_s", True),
    ("renaissance", "avg_ms", False),
]


def rows_by_label(results, suite):
    data = results.get(suite)
    if not isinstance(data, list):
        return {}
    return {r["label"]: r for r in data if isinstance(r, dict) and "label" in r}


def collect(old, new):
    """Yield (name, old value, new value, higher_is_better)."""
    for suite, metric, higher_better in ROW_METRICS:
        old_rows = rows_by_label(old, suite)
        new_rows = rows_by_label(new, suite)
        for label in new_rows:
            if label not in old_rows:
                continue
            o_row, n_row = old_rows[label], new_rows[label]
            # Never compare jOPS across run modes, even if a label matches.
            if suite == "specjbb" and o_row.get("mode") != n_row.get("mode"):
                continue
            # Never compare across Renaissance benchmarks, even if a label
            # somehow matches (labels are benchmark names, so this is the
            # belt-and-suspenders twin of the specjbb mode rule).
            if suite == "renaissance" and o_row.get("benchmark") != n_row.get("benchmark"):
                continue
            o, n = o_row.get(metric), n_row.get(metric)
            if isinstance(o, (int, float)) and isinstance(n, (int, float)):
                yield f"{suite}/{label} {metric}", o, n, higher_better

    old_crit = old.get("criterion") or {}
    new_crit = new.get("criterion") or {}
    for name in new_crit:
        if name in old_crit:
            yield f"criterion/{name} ns", old_crit[name], new_crit[name], False


def main():
    with open(sys.argv[1]) as f:
        old = json.load(f)
    with open(sys.argv[2]) as f:
        new = json.load(f)
    threshold = float(sys.argv[3]) if len(sys.argv) > 3 else DEFAULT_THRESHOLD

    lines = []
    flagged = 0
    for name, o, n, higher_better in collect(old, new):
        if not o:  # zero/missing baseline → delta undefined
            continue
        delta = (n - o) / o * 100
        regressed = delta < -threshold if higher_better else delta > threshold
        improved = delta > threshold if higher_better else delta < -threshold
        mark = "⚠️ regression" if regressed else ("✅ improved" if improved else "")
        if regressed:
            flagged += 1
        lines.append(f"| {name} | {o:g} | {n:g} | {delta:+.1f}% | {mark} |")

    print("## Comparison with previous run\n")
    if not lines:
        print("_No comparable metrics found (first run with this format?)._")
        return
    old_meta = old.get("meta") or {}
    print(
        f"_Baseline: `{old_meta.get('git', '?')}` at"
        f" {old_meta.get('timestamp', '?')} — threshold ±{threshold:g}%._\n"
    )
    print("| Metric | Previous | Current | Δ | |")
    print("|---|---:|---:|---:|---|")
    for line in lines:
        print(line)
    if flagged:
        print(f"\n**{flagged} metric(s) regressed beyond {threshold:g}%.**")


if __name__ == "__main__":
    main()
