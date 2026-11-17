#!/usr/bin/env bash
# Run the whole study one rival at a time, two runs each, then rebuild the results.
# Usage: run-all.sh [RUNS]   (default 2)
set -uo pipefail
cd "$(dirname "$0")"
RUNS=${1:-2}
LLAMA_TAG=$(curl -fsSL 'https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10' |
  python3 -I -c 'import json,sys,re
for r in json.load(sys.stdin):
    if any(re.fullmatch(r"llama-b\d+-bin-ubuntu-x64\.tar\.gz", a["name"]) for a in r["assets"]):
        print(r["tag_name"]); break')
docker pull -q python:3.13-alpine >/dev/null
./netspeed.sh
for r in $(seq 1 "$RUNS"); do
  ./ollama.sh normalized "$r"
  ./ollama.sh quickstart "$r"
  ./llamacpp.sh release "$r" "$LLAMA_TAG"
  ./llamacpp.sh app "$r"
  ./lmstudio.sh "$r"
  ./openwebui.sh "$r"
  ./gpt4all.sh "$r"
  ./gui-installers.sh "$r"
done
python3 -I collect.py
