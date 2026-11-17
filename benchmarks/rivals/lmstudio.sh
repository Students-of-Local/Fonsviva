#!/usr/bin/env bash
# LM Studio headless (llmster): install.sh, lms daemon up, lms get, lms load,
# lms server start, then the first answer through its OpenAI endpoint.
# Usage: lmstudio.sh RUN
source "$(dirname "$0")/lib.sh"
init_run lmstudio llmster "$1"
# lms get takes a Hugging Face URL, so every rival gets the same GGUF file.
GET_ARG="https://huggingface.co/$NORM_HF"
meta model "$NORM_HF ($NORM_HF_FILE)"
meta model_source "huggingface.co/$NORM_HF/$NORM_HF_FILE"
start_container
LMS=/root/.lmstudio/bin/lms

phase prereq "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates"

act "curl -fsSL https://lmstudio.ai/install.sh | bash"
if ! phase install "curl -fsSL https://lmstudio.ai/install.sh | bash"; then
  PKG=$(grep -oE 'apt-get install -y [a-z0-9.+-]+' "$OUT/install.log" | head -1 | awk '{print $NF}')
  [ -n "$PKG" ] || { finish; exit 1; }
  note "install.sh stopped on a clean ubuntu:24.04: 'Missing required dependency' ($PKG). It prints the apt command and asks to run the installer again."
  act "apt-get install -y $PKG (printed by the installer)"
  phase install_dep "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $PKG"
  act "curl -fsSL https://lmstudio.ai/install.sh | bash (again)"
  phase install_retry "curl -fsSL https://lmstudio.ai/install.sh | bash"
fi
grep -q 'Could not detect libgomp' "$OUT"/install*.log &&
  note "install.sh only warned 'Could not detect libgomp. You might need to install libgomp' and reported success."
act "restart the shell or export PATH (installer: 'To add lms to your PATH, either restart your shell or run export PATH=...')"

act "lms daemon up"
phase serve "$LMS daemon up"
meta version "$(docker exec "$C" bash -lc "$LMS version 2>&1 | tr '\n' ' '")"

act "lms get $GET_ARG"
if ! phase download "$LMS get '$GET_ARG' --yes"; then
  note "lms get exited with an error partway through the download ($(tr '\r' '\n' <"$OUT/download.log" | grep -oE '[0-9.]+%' | tail -1)); ran it again."
  act "lms get $GET_ARG (again, after it failed)"
  phase download_retry "$LMS get '$GET_ARG' --yes"
fi
note "lms get and lms load ran with --yes so no confirmation prompt could block; without it they may ask to confirm."
act "lms ls (docs: find the model_key with lms ls)"
docker exec "$C" bash -lc "$LMS ls" >"$OUT/lms-ls.txt" 2>&1 || true
KEY=$(docker exec "$C" bash -lc "$LMS ls --json" 2>/dev/null | python3 -I -c 'import json,sys; m=[x for x in json.load(sys.stdin) if x.get("type")=="llm"]; print(m[0]["modelKey"])')
meta model_key "$KEY"

act "lms load $KEY"
phase load "$LMS load '$KEY' --yes" || true
if grep -q 'exited before becoming healthy' "$OUT/load.log"; then
  note "lms load failed: 'Engine protocol runtime llama-server ... exited before becoming healthy. exitCode=127' (missing libgomp). The error does not name the library; the installer's warning did."
  act "apt-get install libgomp1"
  act "lms load $KEY (again)"
  phase load_fix "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libgomp1"
  phase load_retry "$LMS load '$KEY' --yes"
fi
act "lms server start"
phase serve_http "$LMS server start"
act "send the prompt (curl to http://localhost:1234/v1/chat/completions, or lms chat)"
first_answer "http://127.0.0.1:1234/v1/chat/completions" "$KEY"

docker exec "$C" bash -lc "$LMS ps" >"$OUT/lms-ps.txt" 2>&1 || true
docker exec "$C" bash -lc "$LMS ps --json" >"$OUT/lms-ps.json" 2>&1 || true
docker exec "$C" bash -lc "$LMS ls --json" >"$OUT/lms-ls.json" 2>&1 || true
meta default_context "$(python3 -I -c 'import json,sys; m=[x for x in json.load(open(sys.argv[1])) if x.get("type")=="llm"]; print(m[0].get("contextLength",""))' "$OUT/lms-ps.json" 2>/dev/null)"
meta context_source "lms ps --json contextLength (CONTEXT column in lms ps); maxContextLength is the model maximum"
meta model_file_bytes "$(python3 -I -c 'import json,sys; m=[x for x in json.load(open(sys.argv[1])) if x.get("type")=="llm"]; print(m[0].get("sizeBytes",""))' "$OUT/lms-ls.json" 2>/dev/null)"
cat "$OUT/lms-ps.txt" >&2
finish
