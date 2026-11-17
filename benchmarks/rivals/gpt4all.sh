#!/usr/bin/env bash
# GPT4All Python SDK, the one headless path GPT4All documents (docs.gpt4all.io/gpt4all_python).
# The desktop app is GUI only; see gui-installers.sh.
# Usage: gpt4all.sh RUN [MODEL_NAME]
#   MODEL_NAME defaults to the smallest Q4 instruct model in GPT4All's own catalog;
#   the SDK only downloads catalog models by name.
source "$(dirname "$0")/lib.sh"
init_run gpt4all python-sdk "$1"
MODEL=${2:-Llama-3.2-1B-Instruct-Q4_0.gguf}
meta model "$MODEL"
meta model_source "GPT4All catalog (gpt4all.io models3.json)"
note "The normalized gemma-3-1b-it-Q4_K_M.gguf does not load in the SDK: 'RuntimeError: Unable to instantiate model: Unsupported model architecture: gemma3' (separate untimed probe, gpt4all 2.8.2), so the closest catalog model was used."
start_container

phase prereq "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates"

act "apt-get install python3-venv (docs: install into its own virtual environment)"
phase install_python "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3 python3-venv"
note "python3 is preinstalled on Ubuntu desktop but not in the container, so install_python includes it."
act "python3 -m venv venv"
act "source venv/bin/activate"
act "pip install gpt4all"
phase install "python3 -m venv /root/venv && /root/venv/bin/pip install -q gpt4all"
meta version "gpt4all $(docker exec "$C" /root/venv/bin/pip show gpt4all 2>/dev/null | awk '/^Version/{print $2}')"

act "write chat.py from the docs example"
docker cp "$HERE/gpt4all_chat.py" "$C:/root/chat.py" >/dev/null
act "python chat.py"
phase download "/root/venv/bin/python -c 'from gpt4all import GPT4All; GPT4All.retrieve_model(\"$MODEL\", allow_download=True)'"

r0=$(rx "$C")
rc=0
docker exec "$C" /root/venv/bin/python /root/chat.py "$MODEL" "$PROMPT" "$MAX_TOKENS" >"$OUT/ttft.json" 2>"$OUT/chat.err" || rc=$?
r1=$(rx "$C")
cat "$OUT/ttft.json" >&2
read -r L0 L1 TS TE < <(python3 -I -c 'import json,sys; j=json.load(open(sys.argv[1])); print(j["t_load0"], j["t_load1"], j["t_send"], j["t_send"] + j["total_ms"] / 1000)' "$OUT/ttft.json" 2>/dev/null || echo "0 0 0 0")
record load "$L0" "$L1" 0 "$rc"
record first_answer "$TS" "$TE" $((r1 - r0)) "$rc"
meta default_context "$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1])).get("n_ctx") or "2048 (SDK default n_ctx, not read back)")' "$OUT/ttft.json" 2>/dev/null)"
meta context_source "GPT4All(...).model.n_ctx"
meta model_file_bytes "$(docker exec "$C" bash -lc "find /root/.cache/gpt4all -name '$MODEL' -printf '%s'" 2>/dev/null)"
finish
