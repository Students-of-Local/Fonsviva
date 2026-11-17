"""Turn one run's logs into result.json.

Usage: python3 -I emit.py LOG_DIR

Phase names group by prefix: prereq (not counted, it only installs curl),
install, serve, download, load, first_answer.
"""

import json
import os
import sys


def read_tsv(path):
    rows = []
    with open(path) as f:
        for line in f:
            line = line.rstrip("\n")
            if line:
                rows.append(line.split("\t"))
    return rows


def lines(path):
    with open(path) as f:
        return [l.rstrip("\n") for l in f if l.strip()]


def main():
    out = sys.argv[1]
    meta = {}
    for k, v in read_tsv(os.path.join(out, "meta.tsv")):
        meta[k] = v
    phases = []
    for name, t0, t1, nbytes, rc in read_tsv(os.path.join(out, "phases.tsv")):
        phases.append({"name": name, "t0": float(t0), "t1": float(t1), "s": round(float(t1) - float(t0), 2),
                       "bytes": int(nbytes), "rc": int(rc)})

    def group(prefix):
        ps = [p for p in phases if p["name"].startswith(prefix)]
        return round(sum(p["s"] for p in ps), 2), sum(p["bytes"] for p in ps)

    counted = [p for p in phases if not p["name"].startswith("prereq")]
    ttft = {}
    tpath = os.path.join(out, "ttft.json")
    if os.path.exists(tpath) and os.path.getsize(tpath):
        with open(tpath) as f:
            ttft = json.load(f)

    install_s, install_b = group("install")
    serve_s, serve_b = group("serve")
    download_s, download_b = group("download")
    load_s, load_b = group("load")
    prereq_s, prereq_b = group("prereq")
    uncapped = {}
    upath = os.path.join(out, "ttft_uncapped.json")
    if os.path.exists(upath) and os.path.getsize(upath):
        with open(upath) as f:
            uncapped = json.load(f)
    t_start = counted[0]["t0"] if counted else None
    # The first token of any kind is what a user sees first ("ollama run" prints
    # thinking text too), so totals end there. Visible-answer times are kept apart.
    t_first = ttft.get("t_first_any") or ttft.get("t_first_content")
    total = round(t_first - t_start, 2) if t_start and t_first else None

    actions = lines(os.path.join(out, "actions.txt"))
    result = {
        "rival": meta.get("rival"),
        "variant": meta.get("variant"),
        "run": int(meta.get("run", 0)),
        "date": meta.get("date"),
        "host": meta.get("host"),
        "host_load_start": meta.get("host_load_start"),
        "host_load_end": meta.get("host_load_end"),
        "version": meta.get("version"),
        "measured": meta.get("measured", "headless"),
        "actions": len(actions),
        "actions_list": actions,
        "prereq_s": prereq_s,
        "prereq_bytes": prereq_b,
        "install_s": install_s,
        "install_bytes": install_b,
        "engine_start_s": serve_s,
        "download_s": download_s,
        "download_bytes": download_b + serve_b + load_b,
        "load_s": load_s,
        "ttft_ms": ttft.get("first_any_ms") or ttft.get("ttft_ms"),
        "first_content_ms": ttft.get("ttft_ms"),
        "answered_within_cap": bool(ttft.get("answered", ttft.get("ok"))),
        "uncapped_first_content_ms": uncapped.get("ttft_ms"),
        "uncapped_answer": uncapped.get("answer"),
        "uncapped_completion_tokens": uncapped.get("completion_tokens"),
        "gen_tok_s": ttft.get("gen_tps"),
        "completion_tokens": ttft.get("completion_tokens"),
        "answer": ttft.get("answer"),
        "total_s_to_first_token": total,
        "total_bytes": sum(p["bytes"] for p in counted),
        "model": meta.get("model"),
        "model_source": meta.get("model_source"),
        "model_file_bytes": int(meta["model_file_bytes"]) if meta.get("model_file_bytes") else None,
        "default_context": int(meta["default_context"]) if meta.get("default_context", "").isdigit() else meta.get("default_context"),
        "context_source": meta.get("context_source"),
        "ok": bool(ttft.get("ok")),
        "reasoning_chars": ttft.get("reasoning_chars"),
        "failed_steps": [p["name"] for p in counted if p["rc"] != 0],
        "phases": phases,
        "notes": lines(os.path.join(out, "notes.txt")),
    }
    with open(os.path.join(out, "result.json"), "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps({k: result[k] for k in ("rival", "variant", "run", "ok", "actions", "install_s", "download_s",
                                              "load_s", "ttft_ms", "gen_tok_s", "total_s_to_first_token",
                                              "total_bytes", "model", "default_context")}))


if __name__ == "__main__":
    main()
