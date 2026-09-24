"""Summarise run.py results and the resource samples of sample.sh.

Usage: python3 analyze.py results.json samples.txt
Round 0 of each run is a warm-up and is left out. Sample columns: time, then
RSS (KiB) and CPU seconds of TurboCI, then the same for gitlab-runner.
"""
import json, statistics as st, sys
R = json.load(open(sys.argv[1])); S = [list(map(float, l.split())) for l in open(sys.argv[2]) if len(l.split()) == 5]
runs = [r for r in R if r["round"] > 0]
assert all(r["status"] == "success" for r in R), "failed pipeline"
names = ["noop", "upload", "cache-job", "download", "service"]
out = {}
for tag in ("turboci", "bench-glr"):
    rs = [r for r in runs if r["runner"] == tag]
    d = {"n": len(rs), "wall_median": st.median(r["wall"] for r in rs), "wall_mean": st.mean(r["wall"] for r in rs)}
    for n in names:
        d[n] = st.median(r["jobs"][n]["duration"] for r in rs)
        d[n + "_queued"] = st.median(r["jobs"][n]["queued"] for r in rs)
    d["queued_all_median"] = st.median(r["jobs"][n]["queued"] for r in rs for n in names)
    d["jobs_sum_median"] = st.median(sum(r["jobs"][n]["duration"] for n in names) for r in rs)
    # resources inside this runner's pipeline windows
    col = 1 if tag == "turboci" else 3
    rss, cpu = [], 0.0
    for r in rs:
        w = [s for s in S if r["created"] - 1 <= s[0] <= r["finished"] + 1]
        if len(w) >= 2:
            rss += [s[col] for s in w]
            cpu += w[-1][col + 1] - w[0][col + 1]
    d["rss_peak_mb"] = max(rss) / 1024; d["rss_median_mb"] = st.median(rss) / 1024
    d["cpu_s_per_pipeline"] = cpu / len(rs)
    out[tag] = d
# idle: samples outside every pipeline window
busy = [(r["created"] - 2, r["finished"] + 2) for r in R]
idle = [s for s in S if not any(a <= s[0] <= b for a, b in busy)]
out["idle"] = {"samples": len(idle), "turboci_rss_mb": st.median(s[1] for s in idle) / 1024 if idle else None,
               "glr_rss_mb": st.median(s[3] for s in idle) / 1024 if idle else None}
out["totals"] = {"span_s": S[-1][0] - S[0][0], "turboci_cpu_s": S[-1][2] - S[0][2], "glr_cpu_s": S[-1][4] - S[0][4]}
print(json.dumps(out, indent=1))
