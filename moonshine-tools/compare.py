#!/usr/bin/env python3
"""Compare repeated moonshine-bench runs; Python standard library only.

Percentiles are compared as distributions of per-run values, never averaged
into a pretend pooled percentile. A faster run with worse delivery is flagged.
"""
import argparse
import json
import statistics
import sys
from pathlib import Path


def load(directory):
    groups = {}
    for path in sorted(Path(directory).glob("run-*.json")):
        doc = json.loads(path.read_text())
        if doc.get("schema_version") != 1:
            raise ValueError(f"{path}: unsupported schema")
        if not doc.get("valid"):
            raise ValueError(f"{path}: invalid run: {doc.get('invalid_reasons', doc.get('error'))}")
        key = json.dumps(doc["config"], sort_keys=True)
        groups.setdefault(key, []).append(doc)
    if not groups:
        raise ValueError(f"{directory}: no run reports")
    return groups


def environment_key(doc):
    env = doc["environment"]
    return json.dumps({k: env.get(k) for k in (
        "kernel", "cpu", "available_render_devices", "nvidia_driver", "debug_assertions", "runtime_gpu_settings"
    )}, sort_keys=True)


def percent(value, total):
    return 100 * value / total if total else 0.0


def measures(doc):
    summary = doc["summary"]
    stages = summary["stages_us"]
    counters = doc["counters"]
    def quantile(stage, q):
        value = stages.get(stage, {}).get(q)
        if value is None:
            raise ValueError(f"missing/overflowed {stage}.{q} in run {doc['run']}")
        return value
    result = {
        "host p50 us": quantile("host_total", "p50"),
        "host p95 us": quantile("host_total", "p95"),
        "host p99 us": quantile("host_total", "p99"),
        "loopback p99 us": quantile("loopback_complete", "p99"),
        "network queue p99 us": quantile("network_queue", "p99"),
        "successful FPS": summary["successful_fps"],
        "over budget %": percent(summary["over_frame_budget"], summary["successful_frames"]),
        "capture pressure %": percent(sum(counters.get(k, 0) for k in (
            "pool_busy", "capture_queue_full", "encode_backpressure"
        )), counters.get("capture_attempts", 0)),
        "incomplete receipt %": percent(summary["incomplete_receipts"], summary["successful_frames"]),
        "socket failed frames": counters.get("send_failed_frames", 0),
        "outstanding frames at end": doc.get("outstanding_submitted_frames_end", 0),
        "pipeline errors": sum(counters.get(k, 0) for k in (
            "capture_errors", "import_errors", "convert_errors", "submit_errors", "readback_errors", "packetize_errors"
        )),
    }

    for stage in ("scene_wait", "buffer_age", "buffer_to_send"):
        if stage in stages:
            for q in ("p50", "p99"):
                result[f"{stage} {q} us"] = quantile(stage, q)
    return result


def compare(baseline, candidate, allow_environment=False, fail_percent=None):
    if baseline.keys() != candidate.keys():
        raise ValueError("workload/configuration sets differ (including duration, warmup, raw and verbose); compare matching runs")
    regression = False
    lines = []
    for key in sorted(baseline):
        before, after = baseline[key], candidate[key]
        if not allow_environment and len({environment_key(d) for d in before + after}) != 1:
            raise ValueError("hardware/driver/kernel metadata differs; use --allow-environment-change for an intentional comparison")
        config = json.loads(key)
        lines.append(f"\n{config['resolution']} {config['fps']} FPS {config['codec']} — {config['workload']}")
        lines.append(f"Runs: {len(before)} baseline, {len(after)} candidate")
        if min(len(before), len(after)) < 3:
            lines.append("CAUTION: fewer than three repetitions; run-to-run noise is poorly characterized.")
        lines.append("Values: median [minimum, maximum] across runs; changes are descriptive, not a significance test.")
        left, right = [measures(d) for d in before], [measures(d) for d in after]
        common = set.intersection(*(set(d) for d in left + right))
        missing = set.union(*(set(d) for d in left + right)) - common
        if missing:
            lines.append("CAUTION: optional timing metrics absent in some runs; omitted: " + ", ".join(sorted(missing)))
        lines.append("")
        lines.append("| Metric | Baseline | Candidate | Change |")
        lines.append("|---|---:|---:|---:|")
        for name in (name for name in left[0] if name in common):
            a, b = [d[name] for d in left], [d[name] for d in right]
            ma, mb = statistics.median(a), statistics.median(b)
            delta = (mb - ma) / ma * 100 if ma else None
            change = f"{delta:+.1f}%" if delta is not None else ("unchanged" if mb == 0 else "increased from zero")
            lines.append(f"| {name} | {ma:.2f} [{min(a):.2f}, {max(a):.2f}] | {mb:.2f} [{min(b):.2f}, {max(b):.2f}] | {change} |")
            worse = mb < ma if name == "successful FPS" else mb > ma
            if fail_percent is not None and worse:
                if ma == 0 or abs(delta) > fail_percent:
                    regression = True
        quality = ["capture pressure %", "incomplete receipt %", "socket failed frames", "pipeline errors"]
        if any(statistics.median(d[k] for d in right) > statistics.median(d[k] for d in left) for k in quality):
            lines.append("CAUTION: delivery/drop/error metrics worsened. Lower successful-frame latency alone is not an improvement.")
    return "\n".join(lines), regression


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline")
    parser.add_argument("candidate")
    parser.add_argument("--allow-environment-change", action="store_true")
    parser.add_argument("--fail-percent", type=float, help="Exit 2 when any median metric regresses by more than this percentage")
    args = parser.parse_args()
    if args.fail_percent is not None and args.fail_percent < 0:
        parser.error("--fail-percent must be nonnegative")
    try:
        output, regression = compare(load(args.baseline), load(args.candidate), args.allow_environment_change, args.fail_percent)
    except (ValueError, KeyError, OSError) as exc:
        parser.exit(1, f"Cannot compare: {exc}\n")
    print(output)
    return 2 if regression else 0


if __name__ == "__main__":
    sys.exit(main())
