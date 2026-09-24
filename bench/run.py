"""Run the benchmark pipeline alternately on two runners and record GitLab's timings.

Usage:
    GITLAB_URL=https://gitlab.example.com GITLAB_TOKEN=glpat-... PROJECT_ID=42 \
        python3 run.py ROUNDS results.json

The project's REF branch (default "bench") must hold bench/gitlab-ci.yml as
.gitlab-ci.yml. TAGS (default "turboci,bench-glr") are the two runners' tags;
the order alternates every round so neither runner always goes first.
"""
import json, os, sys, time, urllib.request
from datetime import datetime

T = os.environ["GITLAB_TOKEN"]
A = f"{os.environ['GITLAB_URL'].rstrip('/')}/api/v4/projects/{os.environ['PROJECT_ID']}"
REF = os.environ.get("REF", "bench")
TAGS = os.environ.get("TAGS", "turboci,bench-glr").split(",")


def api(path, method="GET", body=None):
    # Status polls are retried: one network hiccup must not lose a whole run
    for attempt in range(10):
        try:
            return _api(path, method, body)
        except Exception as e:
            if method != "GET" or attempt == 9:
                raise
            print("retry", path, e, file=sys.stderr, flush=True)
            time.sleep(5)


def _api(path, method="GET", body=None):
    req = urllib.request.Request(A + path, method=method, data=body and json.dumps(body).encode(),
                                 headers={"PRIVATE-TOKEN": T, "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


rounds = int(sys.argv[1])
out = []
for rnd in range(rounds):
    for tag in (TAGS if rnd % 2 == 0 else TAGS[::-1]):
        p = api("/pipeline", "POST", {"ref": REF, "variables": [{"key": "BENCH_TAG", "value": tag}]})
        while True:
            time.sleep(3)
            p = api(f"/pipelines/{p['id']}")
            if p["status"] in ("success", "failed", "canceled"):
                break
        jobs = api(f"/pipelines/{p['id']}/jobs?per_page=50")
        rec = {"round": rnd, "runner": tag, "pipeline": p["id"], "status": p["status"],
               "created": ts(p["created_at"]), "finished": ts(p["finished_at"]),
               "wall": ts(p["finished_at"]) - ts(p["created_at"]),
               "jobs": {j["name"]: {"status": j["status"], "duration": j["duration"],
                                    "queued": j["queued_duration"]} for j in jobs}}
        out.append(rec)
        json.dump(out, open(sys.argv[2], "w"), indent=1)
        print(json.dumps({k: rec[k] for k in ("round", "runner", "pipeline", "status", "wall")}), flush=True)
        time.sleep(5)
