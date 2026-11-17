"""Rebuild results.json and RESULTS.md from logs/.

Usage: python3 -I collect.py [DIR]   (DIR defaults to this script's folder)

Re-runs emit.py on every run so all results use the same rules, then writes
one object per rival per run to results.json and a median table to RESULTS.md.
"""

import glob
import json
import os
import statistics
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = sys.argv[1] if len(sys.argv) > 1 else HERE

ORDER = [
    ("ollama", "normalized"), ("ollama", "quickstart"),
    ("llamacpp", "release"), ("llamacpp", "app"),
    ("lmstudio", "llmster"), ("openwebui", "bundled-ollama"),
    ("gpt4all", "python-sdk"),
    ("jan", "gui-installer"), ("gpt4all", "gui-installer"), ("msty", "gui-installer"), ("pinokio", "gui-installer"),
]
LABEL = {
    ("ollama", "normalized"): "Ollama (install.sh), normalized model",
    ("ollama", "quickstart"): "Ollama (install.sh), docs quickstart model",
    ("llamacpp", "release"): "llama.cpp, GitHub release tarball + llama-server",
    ("llamacpp", "app"): "llama.cpp, llama.app installer + llama serve (README today)",
    ("lmstudio", "llmster"): "LM Studio headless (llmster + lms)",
    ("openwebui", "bundled-ollama"): "Open WebUI :ollama image (Docker)",
    ("gpt4all", "python-sdk"): "GPT4All Python SDK",
    ("jan", "gui-installer"): "Jan",
    ("gpt4all", "gui-installer"): "GPT4All desktop",
    ("msty", "gui-installer"): "Msty Studio",
    ("pinokio", "gui-installer"): "Pinokio",
}


# What needs sudo on a real Ubuntu machine (the containers ran as root).
SUDO = {
    ("ollama", "normalized"): "yes: install.sh writes /usr/local (asks for the password); zstd via apt",
    ("ollama", "quickstart"): "yes: install.sh writes /usr/local (asks for the password); zstd via apt",
    ("llamacpp", "release"): "only if libgomp1 is missing (apt)",
    ("llamacpp", "app"): "no",
    ("lmstudio", "llmster"): "only if libatomic1 or libgomp1 is missing (apt)",
    ("openwebui", "bundled-ollama"): "Docker install and docker group",
    ("gpt4all", "python-sdk"): "yes: python3-venv via apt",
    ("jan", "gui-installer"): "AppImage no (libfuse2 via apt if missing); .deb and Flatpak setup yes",
    ("gpt4all", "gui-installer"): "no (installs to home)",
    ("msty", "gui-installer"): "AppImage no (libfuse2 via apt if missing); .deb yes",
    ("pinokio", "gui-installer"): "AppImage no (libfuse2 via apt if missing); .deb yes",
}


def load1(r, key):
    v = r.get(key)
    return float(v.split()[0]) if v else None


def med(values):
    vals = [v for v in values if isinstance(v, (int, float))]
    return statistics.median(vals) if vals else None


def fmt(v, digits=1):
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:,.{digits}f}"
    return f"{v:,}"


def mb(v):
    return "n/a" if v is None else f"{v / 1e6:,.0f} MB"


def main():
    results = []
    for d in sorted(glob.glob(os.path.join(ROOT, "logs", "*", "run*"))):
        if not os.path.exists(os.path.join(d, "meta.tsv")):
            continue
        subprocess.run([sys.executable, "-I", os.path.join(HERE, "emit.py"), d], check=True, stdout=subprocess.DEVNULL)
        with open(os.path.join(d, "result.json")) as f:
            r = json.load(f)
        r["log_dir"] = os.path.relpath(d, ROOT)
        results.append(r)
    results.sort(key=lambda r: (ORDER.index((r["rival"], r["variant"])) if (r["rival"], r["variant"]) in ORDER else 99, r["run"]))
    with open(os.path.join(ROOT, "results.json"), "w") as f:
        json.dump(results, f, indent=2)

    net = {}
    npath = os.path.join(ROOT, "logs", "netspeed", "result.json")
    if os.path.exists(npath):
        with open(npath) as f:
            net = json.load(f)

    groups = {}
    for r in results:
        groups.setdefault((r["rival"], r["variant"]), []).append(r)

    lines = []
    lines.append("# Rivals: clean machine to first local answer (Linux, headless)")
    lines.append("")
    lines.append(f"Measured {results[0]['date'][:10] if results else ''} on abbey1 (WSL2 Ubuntu host, AMD Ryzen 5 3600, 12 threads). "
                 "Each run starts from a fresh `ubuntu:24.04` container with only curl and ca-certificates added "
                 "(that prerequisite step is timed but not counted). Containers are limited to 8 CPUs and 12 GB, CPU only. "
                 "Two runs per rival, one at a time, each started only while abbey1's 1-minute load was 10 or less; "
                 "numbers are the median (the mean of the two). abbey1 is a shared host, so its load is listed beside every row. "
                 "All runs are CPU only: untested on NVIDIA.")
    lines.append("")
    if net:
        lines.append(f"Network baseline: {net['file_bytes']:,} bytes from `{net['url']}` in {net['median_seconds']:.2f} s "
                     f"(median of {len(net['samples'])}), {net['median_MB_per_s']} MB/s, about {net['median_Mbit_per_s']} Mbit/s"
                     + (f" (abbey1 load {net['host_load_start']} at start)." if net.get("host_load_start") else "."))
        lines.append("")
    lines.append("Normalized model: `gemma3:1b` from the Ollama library and `ggml-org/gemma-3-1b-it-GGUF` "
                 "(`gemma-3-1b-it-Q4_K_M.gguf`, 806,058,240 bytes) from Hugging Face; the Ollama blob is the same size within 160 bytes. "
                 "Prompt: \"Name three primary colors.\", max 64 tokens, temperature 0, streamed.")
    lines.append("")
    lines.append("TTFT is measured from the request send to the first streamed token, on a warm model after load. "
                 "Total is wall clock from the first install command to that first token (installer, engine start, "
                 "model download, load and first answer, including the time lost to failed steps).")
    lines.append("")
    lines.append("| Rival | Actions | Install s | Download bytes | Model | Default context | TTFT ms | Gen tok/s | Total s to first token | abbey1 load (1 min, start/end) | Needs sudo on a real machine | Notes |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for key in ORDER:
        rs = groups.get(key)
        if not rs:
            continue
        r0 = rs[0]
        notes = []
        if r0.get("measured", "headless") != "headless":
            notes.append(r0["measured"])
        if any(not r["ok"] for r in rs) and r0.get("measured", "headless") == "headless":
            notes.append(f"{sum(1 for r in rs if not r['ok'])} of {len(rs)} runs got no answer")
        if r0.get("failed_steps"):
            notes.append("failed first: " + ", ".join(sorted(set(s for r in rs for s in r.get("failed_steps", [])))))
        if r0.get("uncapped_first_content_ms") or any(not r.get("answered_within_cap", True) for r in rs if r["ok"]):
            notes.append("thinking model: no visible answer within 64 tokens; first visible answer after "
                         f"{fmt(med(r.get('uncapped_first_content_ms') for r in rs), 0)} ms uncapped")
        total_bytes = med(r["total_bytes"] for r in rs)
        lines.append("| " + " | ".join([
            LABEL.get(key, f"{key[0]} {key[1]}"),
            fmt(med(r["actions"] for r in rs), 0) if isinstance(med(r["actions"] for r in rs), float) else str(med(r["actions"] for r in rs)),
            fmt(med(r["install_s"] for r in rs)),
            mb(total_bytes),
            (r0.get("model") or "n/a").replace("|", "/"),
            fmt(r0.get("default_context")) if r0.get("default_context") not in (None, "") else "n/a",
            fmt(med(r.get("ttft_ms") for r in rs), 0),
            fmt(med(r.get("gen_tok_s") for r in rs)),
            fmt(med(r.get("total_s_to_first_token") for r in rs)),
            ", ".join(f"{load1(r, 'host_load_start')}/{load1(r, 'host_load_end')}" for r in rs),
            SUDO.get(key, "n/a"),
            "; ".join(notes) or "",
        ]) + " |")
    lines.append("")
    lines.append("## Phase breakdown (median)")
    lines.append("")
    lines.append("| Rival | Install s | Installer bytes | Engine start s | Model download s | Model bytes | Load s | First answer tokens | Runs |")
    lines.append("|---|---|---|---|---|---|---|---|---|")
    for key in ORDER:
        rs = groups.get(key)
        if not rs:
            continue
        if rs[0].get("measured", "headless") != "headless":
            lines.append("| " + " | ".join([
                LABEL.get(key, f"{key[0]} {key[1]}"),
                fmt(med(r["install_s"] for r in rs)),
                mb(med(r["install_bytes"] for r in rs)),
                "n/a", "n/a", "n/a", "n/a", "n/a",
                ", ".join(f"{fmt(r['install_s'])} s download" for r in rs),
            ]) + " |")
            continue
        lines.append("| " + " | ".join([
            LABEL.get(key, f"{key[0]} {key[1]}"),
            fmt(med(r["install_s"] for r in rs)),
            mb(med(r["install_bytes"] for r in rs)),
            fmt(med(r["engine_start_s"] for r in rs)),
            fmt(med(r["download_s"] for r in rs)),
            mb(med(r["download_bytes"] for r in rs)),
            fmt(med(r["load_s"] for r in rs)),
            fmt(med(r.get("completion_tokens") for r in rs), 0),
            ", ".join(f"{fmt(r.get('total_s_to_first_token'))} s" for r in rs),
        ]) + " |")
    lines.append("")
    lines.append("## Actions and what went wrong")
    lines.append("")
    for key in ORDER:
        rs = groups.get(key)
        if not rs:
            continue
        r0 = rs[0]
        lines.append(f"### {LABEL.get(key, key[0])}")
        lines.append("")
        if r0.get("version"):
            lines.append(f"Version: `{r0['version'].strip()}`")
            lines.append("")
        lines.append("Actions (run 1):")
        lines.append("")
        for i, a in enumerate(r0["actions_list"], 1):
            lines.append(f"{i}. `{a}`" if a.startswith(("curl", "lms", "ollama", "llama", "apt", "docker", "python", "pip", "tar", "chmod", "source", "./")) else f"{i}. {a}")
        lines.append("")
        notes = []
        for r in rs:
            for n in r.get("notes", []):
                if n not in notes:
                    notes.append(n)
        if notes:
            lines.append("Notes:")
            lines.append("")
            for n in notes:
                lines.append(f"- {n}")
            lines.append("")
        answers = [r.get("answer") for r in rs if r.get("answer")]
        if answers:
            lines.append("First answer (run 1): " + json.dumps(answers[0]))
            lines.append("")
    lines.append("## Reproduce")
    lines.append("")
    lines.append("On a Linux docker host: `./netspeed.sh`, then each rival script with its variant and run number "
                 "(`./ollama.sh normalized 1`, `./llamacpp.sh release 1`, `./lmstudio.sh 1`, `./openwebui.sh 1`, "
                 "`./gpt4all.sh 1`, `./gui-installers.sh 1`), or `./run-all.sh`. Then `python3 -I collect.py`. "
                 "Containers are named `fv-rival-*` and removed when each run ends.")
    lines.append("")
    with open(os.path.join(ROOT, "RESULTS.md"), "w") as f:
        f.write("\n".join(lines))
    print(f"{len(results)} runs -> results.json, RESULTS.md")


if __name__ == "__main__":
    main()
