#!/usr/bin/env python3
"""Parsing and JSON plumbing for scripts/bench.sh.

Subcommands:
  parse <kind> <file>            kind: text | locust-csv | ws-text
                                 prints shell-evalable `key=value` lines
  append <json> <label> [k=v..]  append a row object to a JSON array file
  boundary <kind> <file> <max-vus> <duration-s> <err-pct>
                                 kind: native | k6 | locust | jmeter —
                                 where the ramp run first hit the error-rate
                                 threshold; prints crossed/boundary_* k=v
  setobj <json> [k=v..]          merge keys into a JSON object file
  embed <json> <key> <file>      store a parsed JSON file under a key
  merge <dir> <out>              combine <dir>/*.json into one object keyed
                                 by basename
  startup <hyperfine-json> <out-json> <duration-secs>
                                 wrapper-overhead table from a short-duration
                                 hyperfine run; prints markdown rows
  criterion <criterion-dir> <results-json> <report-md>
                                 collect cargo-bench estimates into the
                                 results file and append a markdown table

`parse` never exits non-zero: on any failure it prints zeroed metrics plus
`parse_ok=0` so one bad scenario doesn't kill the whole bench run.
"""

import json
import os
import re
import sys

METRIC_KEYS = [
    "requests", "rps", "avg_ms", "p50_ms", "p90_ms", "p95_ms", "p99_ms",
    "min_ms", "max_ms", "err_pct",
]

UNIT_MS = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1000.0}
VALUE_UNIT = r"([\d.]+)(ns|µs|us|ms|s)"


def zeroed():
    return {k: 0 for k in METRIC_KEYS}


def parse_text(content):
    """Parse the end-of-run summary shared by k6 and perfscale's uniform
    format (`http_reqs...: N R/s`, `http_req_duration...: avg=..ms p(50)=..`).
    Units differ (k6 auto-scales to µs/ms/s); everything is normalised to ms.
    """
    out = zeroed()

    m = re.search(r"http_reqs[.\s]*:\s*(\d+)\s+([\d.]+)/s", content)
    if m:
        out["requests"] = int(m.group(1))
        out["rps"] = float(m.group(2))

    m = re.search(r"http_req_failed[.\s]*:\s*([\d.]+)%", content)
    if m:
        out["err_pct"] = float(m.group(1))

    m = re.search(r"http_req_duration[.\s]*:(.*)", content)
    if m:
        stats = {}
        for key, value, unit in re.findall(
            r"(avg|min|med|max|p\(50\)|p\(90\)|p\(95\)|p\(99\))=" + VALUE_UNIT,
            m.group(1),
        ):
            stats[key] = float(value) * UNIT_MS[unit]
        out["avg_ms"] = stats.get("avg", 0)
        out["p50_ms"] = stats.get("p(50)", stats.get("med", 0))
        out["p90_ms"] = stats.get("p(90)", 0)
        out["p95_ms"] = stats.get("p(95)", 0)
        out["p99_ms"] = stats.get("p(99)", 0)
        out["min_ms"] = stats.get("min", 0)
        out["max_ms"] = stats.get("max", 0)

    out["parse_ok"] = 1 if out["requests"] else 0
    return out


def parse_ws_text(content):
    """Parse the WebSocket echo-suite metrics: `ws_msgs_sent...: N R/s`
    (counter) and `ws_msg_rtt...: avg=..ms p(50)=..ms ...` (trend). Both the
    native engine and the bench k6 script emit these two lines in k6-summary
    shape, so one parser fits every ws scenario. `requests`/`rps` count
    messages, not HTTP requests.
    """
    out = zeroed()

    m = re.search(r"ws_msgs_sent[.\s]*:\s*(\d+)\s+([\d.]+)/s", content)
    if m:
        out["requests"] = int(m.group(1))
        out["rps"] = float(m.group(2))

    m = re.search(r"ws_msg_rtt[.\s]*:(.*)", content)
    if m:
        stats = {}
        for key, value, unit in re.findall(
            r"(avg|min|med|max|p\(50\)|p\(90\)|p\(95\)|p\(99\))=" + VALUE_UNIT,
            m.group(1),
        ):
            stats[key] = float(value) * UNIT_MS[unit]
        out["avg_ms"] = stats.get("avg", 0)
        out["p50_ms"] = stats.get("p(50)", stats.get("med", 0))
        out["p90_ms"] = stats.get("p(90)", 0)
        out["p95_ms"] = stats.get("p(95)", 0)
        out["p99_ms"] = stats.get("p(99)", 0)
        out["min_ms"] = stats.get("min", 0)
        out["max_ms"] = stats.get("max", 0)

    out["parse_ok"] = 1 if out["requests"] else 0
    return out


def parse_locust_csv(path):
    """Parse the `Aggregated` row of locust's `--csv` stats output —
    the same source perfscale's locust runner reads."""
    import csv as csvmod

    out = zeroed()
    with open(path, newline="") as f:
        rows = list(csvmod.DictReader(f))
    agg = next((r for r in rows if r.get("Name") == "Aggregated"), None)
    if agg is None:
        out["parse_ok"] = 0
        return out

    def col(name):
        try:
            return float(agg.get(name) or 0)
        except ValueError:
            return 0.0

    out["requests"] = int(col("Request Count"))
    out["rps"] = col("Requests/s")
    out["avg_ms"] = col("Average Response Time")
    out["p50_ms"] = col("50%")
    out["p90_ms"] = col("90%")
    out["p95_ms"] = col("95%")
    out["p99_ms"] = col("99%")
    out["min_ms"] = col("Min Response Time")
    out["max_ms"] = col("Max Response Time")
    failures = col("Failure Count")
    out["err_pct"] = failures / out["requests"] * 100 if out["requests"] else 0.0
    out["parse_ok"] = 1 if out["requests"] else 0
    return out


def cmd_parse(kind, path):
    try:
        if kind == "locust-csv":
            metrics = parse_locust_csv(path)
        elif kind == "ws-text":
            with open(path, encoding="utf-8", errors="replace") as f:
                metrics = parse_ws_text(f.read())
        else:
            with open(path, encoding="utf-8", errors="replace") as f:
                metrics = parse_text(f.read())
    except OSError as e:
        print(f"bench_metrics: {e}", file=sys.stderr)
        metrics = zeroed()
        metrics["parse_ok"] = 0

    for k, v in metrics.items():
        print(f"{k}={v:.2f}" if isinstance(v, float) else f"{k}={v}")


# ---------------------------------------------------------------------------
# boundary suite: ramp 0→MAX VUs and find where the error rate first reaches
# BOUNDARY_ERR_PCT (default 1%). One parser per engine's periodic output; all
# report the same shape: crossed=0|1, boundary_vus, boundary_s, boundary_rps.
# crossed=0 means the run never hit the threshold — the boundary lies beyond
# MAX ("<MAX" is rendered by bench.sh).
# ---------------------------------------------------------------------------


def _boundary_out(crossed, vus, secs, rps):
    return {
        "crossed": crossed,
        "boundary_vus": round(vus),
        "boundary_s": round(secs, 1),
        "boundary_rps": round(rps, 2),
    }


def boundary_native(path, max_vus, duration_s, err_pct):
    """Native engine's periodic `[stats] ts=.. rps=.. err_pct=.. .. vus=N`
    lines (one every 5s; err_pct is cumulative, vus= only on staged runs)."""
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = [l for l in f if l.startswith("[stats] ")]
    first_ts = None
    for line in lines:
        m_err = re.search(r"err_pct=([\d.]+)", line)
        if not m_err:  # reqs=0 warmup lines carry no err_pct
            continue
        m_ts = re.search(r"ts=(\d+)", line)
        ts = int(m_ts.group(1)) if m_ts else 0
        if first_ts is None:
            first_ts = ts
        if float(m_err.group(1)) >= err_pct:
            m_vus = re.search(r"vus=(\d+)", line)
            m_rps = re.search(r"rps=([\d.]+)", line)
            secs = (ts - first_ts) / 1000.0 + 5.0  # lines are 5s apart
            vus = int(m_vus.group(1)) if m_vus else max_vus * min(secs, duration_s) / duration_s
            return _boundary_out(1, vus, secs, float(m_rps.group(1)) if m_rps else 0)
    return _boundary_out(0, 0, 0, 0)


def boundary_k6(path, max_vus, duration_s, err_pct):
    """k6 ramping-vus run with `http_req_failed: rate<X abortOnFail`: the
    summary's cumulative error rate tells whether the threshold tripped; the
    last `running (Ts), N/MAX VUs` progress line then gives the exact abort
    point (vus_max is useless here — k6 pre-allocates it to the ramp target)."""
    with open(path, encoding="utf-8", errors="replace") as f:
        content = f.read()
    m_err = re.search(r"http_req_failed[.\s]*:\s*([\d.]+)%", content)
    if not m_err or float(m_err.group(1)) < err_pct:
        return _boundary_out(0, 0, 0, 0)
    progress = re.findall(r"running \((?:(\d+)m)?([\d.]+)s\), (\d+)/(\d+) VUs", content)
    vus = secs = 0
    if progress:
        # The final line post-abort reads 0/MAX VUs (teardown) — take the
        # peak-VU line instead; on a monotonic ramp-up that is the abort point.
        mins, s, vus_s, _ = max(progress, key=lambda m: int(m[2]))
        vus, secs = int(vus_s), int(mins or 0) * 60 + float(s)
    m_it = re.search(r"iterations[.\s]*:\s*\d+\s+([\d.]+)/s", content)
    return _boundary_out(
        1, vus, secs, float(m_it.group(1)) if m_it else 0
    )


def boundary_locust(path, max_vus, duration_s, err_pct):
    """locust `--csv-full-history` stats_history: Aggregated rows carry the
    cumulative Total Request/Failure Counts plus the live User Count."""
    import csv

    with open(path, encoding="utf-8", errors="replace", newline="") as f:
        rows = [r for r in csv.DictReader(f) if r.get("Name") == "Aggregated"]
    if not rows:
        return _boundary_out(0, 0, 0, 0)
    first_ts = int(rows[0]["Timestamp"])
    for r in rows:
        total = int(r.get("Total Request Count") or 0)
        failed = int(r.get("Total Failure Count") or 0)
        if total and failed / total * 100 >= err_pct:
            return _boundary_out(
                1,
                int(r.get("User Count") or 0),
                int(r["Timestamp"]) - first_ts,
                float(r.get("Requests/s") or 0),
            )
    return _boundary_out(0, 0, 0, 0)


def boundary_jmeter(path, max_vus, duration_s, err_pct):
    """jmeter non-GUI `summary +` interval deltas (see
    -Jsummariser.interval=2): errors are accumulated across intervals until
    the cumulative rate reaches the threshold; the VU level is derived from
    the elapsed time on the linear ramp."""
    total = errors = 0
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            m = re.search(
                r"summary \+\s+(\d+) in (\d+):(\d+):(\d+) =\s+([\d.]+)/s.*?Err:\s+(\d+)",
                line,
            )
            if not m:
                continue
            total += int(m.group(1))
            errors += int(m.group(6))
            if total and errors / total * 100 >= err_pct:
                secs = int(m.group(2)) * 3600 + int(m.group(3)) * 60 + int(m.group(4))
                vus = max_vus * min(secs, duration_s) / duration_s
                return _boundary_out(1, vus, secs, float(m.group(5)))
    return _boundary_out(0, 0, 0, 0)


def cmd_boundary(kind, path, max_vus, duration_s, err_pct):
    parsers = {
        "native": boundary_native,
        "k6": boundary_k6,
        "locust": boundary_locust,
        "jmeter": boundary_jmeter,
    }
    try:
        metrics = parsers[kind](path, int(max_vus), float(duration_s), float(err_pct))
    except (OSError, ValueError, KeyError) as e:
        print(f"bench_metrics: boundary: {e}", file=sys.stderr)
        metrics = _boundary_out(0, 0, 0, 0)
    for k, v in metrics.items():
        print(f"{k}={v}")


def coerce(raw):
    for cast in (int, float):
        try:
            return cast(raw)
        except ValueError:
            pass
    return raw


def kv_pairs(args):
    return {k: coerce(v) for k, v in (a.split("=", 1) for a in args)}


def load_json(path, default):
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    return default


def dump_json(path, data):
    with open(path, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")


def cmd_append(path, label, kvs):
    rows = load_json(path, [])
    rows.append({"label": label, **kv_pairs(kvs)})
    dump_json(path, rows)


def cmd_setobj(path, kvs):
    obj = load_json(path, {})
    obj.update(kv_pairs(kvs))
    dump_json(path, obj)


def cmd_embed(path, key, src):
    obj = load_json(path, {})
    with open(src) as f:
        obj[key] = json.load(f)
    dump_json(path, obj)


def cmd_merge(directory, out):
    merged = {}
    for name in sorted(os.listdir(directory)):
        if name.endswith(".json"):
            with open(os.path.join(directory, name)) as f:
                merged[name[: -len(".json")]] = json.load(f)
    dump_json(out, merged)


def duration_secs(s):
    """'30s' / '1m' / '1m30s' / bare seconds → float seconds."""
    total, num = 0.0, ""
    for ch in s.strip():
        if ch.isdigit() or ch == ".":
            num += ch
        elif ch in "hms" and num:
            total += float(num) * {"h": 3600, "m": 60, "s": 1}[ch]
            num = ""
    return total + (float(num) if num else 0.0)


def cmd_startup(hyperfine_json, out_json, duration):
    """Wrapper overhead from a short-duration hyperfine run.

    At a 1s test duration the perfscale wrapper's startup cost is a visible
    fraction of the wall time instead of noise on top of 30s. `vs native`
    subtracts the matching bare engine; `vs ideal` subtracts the configured
    test duration itself (startup + teardown of the whole stack).
    """
    with open(hyperfine_json) as f:
        results = json.load(f)["results"]
    ideal = duration_secs(duration)
    means = {r["command"]: r["mean"] for r in results}

    native_for = {
        "perfscale (k6)": "k6 (native)",
        "perfscale (locust)": "locust (native)",
        "perfscale (jmeter)": "jmeter (native)",
    }

    rows = []
    for r in results:
        name = r["command"]
        native = native_for.get(name)
        overhead_native = means[name] - means[native] if native in means else None
        rows.append({
            "label": name,
            "mean_s": round(r["mean"], 4),
            "stddev_s": round(r.get("stddev") or 0, 4),
            "overhead_vs_native_ms": (
                round(overhead_native * 1000, 1)
                if overhead_native is not None else None
            ),
            "overhead_vs_ideal_ms": round((r["mean"] - ideal) * 1000, 1),
        })

    dump_json(out_json, rows)
    for row in rows:
        vs_native = (
            f"{row['overhead_vs_native_ms']:+.1f} ms"
            if row["overhead_vs_native_ms"] is not None else "—"
        )
        print(
            f"| {row['label']} | {row['mean_s']:.3f} ± {row['stddev_s']:.3f} "
            f"| {vs_native} | {row['overhead_vs_ideal_ms']:+.1f} ms |"
        )


def cmd_criterion(criterion_dir, results_json, report_md):
    """Collect `cargo bench` (criterion) mean estimates.

    Reads target/criterion/<bench>/new/estimates.json, stores nanosecond
    means in the results file, and appends a human table to the report.
    """
    benches = {}
    for name in sorted(os.listdir(criterion_dir)):
        est = os.path.join(criterion_dir, name, "new", "estimates.json")
        if not os.path.exists(est):
            continue
        with open(est) as f:
            data = json.load(f)
        benches[name] = round(data["mean"]["point_estimate"], 1)  # ns

    obj = load_json(results_json, {})
    obj["criterion"] = benches
    dump_json(results_json, obj)

    def fmt(ns):
        if ns >= 1e6:
            return f"{ns / 1e6:.2f} ms"
        if ns >= 1e3:
            return f"{ns / 1e3:.2f} µs"
        return f"{ns:.0f} ns"

    with open(report_md, "a") as f:
        f.write("\n## Micro-benchmarks (criterion)\n\n")
        f.write("| Benchmark | Mean |\n|---|---:|\n")
        for name, ns in benches.items():
            f.write(f"| {name} | {fmt(ns)} |\n")


def main():
    cmd = sys.argv[1]
    if cmd == "parse":
        cmd_parse(sys.argv[2], sys.argv[3])
    elif cmd == "append":
        cmd_append(sys.argv[2], sys.argv[3], sys.argv[4:])
    elif cmd == "setobj":
        cmd_setobj(sys.argv[2], sys.argv[3:])
    elif cmd == "embed":
        cmd_embed(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "merge":
        cmd_merge(sys.argv[2], sys.argv[3])
    elif cmd == "startup":
        cmd_startup(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "criterion":
        cmd_criterion(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "boundary":
        cmd_boundary(sys.argv[2], sys.argv[3], *sys.argv[4:7])
    else:
        sys.exit(f"bench_metrics: unknown subcommand '{cmd}'")


if __name__ == "__main__":
    main()
