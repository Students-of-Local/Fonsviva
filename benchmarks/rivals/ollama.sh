#!/usr/bin/env bash
# Ollama, Linux headless: install.sh, ollama serve, then the first answer.
# Usage: ollama.sh quickstart|normalized RUN
#   quickstart: gemma4:e2b, the model docs.ollama.com/quickstart names ("Run a model locally")
#   normalized: gemma3:1b (Q4_K_M), the model every rival runs
source "$(dirname "$0")/lib.sh"
VARIANT=$1
case "$VARIANT" in
  quickstart) MODEL=gemma4:e2b ;;
  normalized) MODEL=$NORM_OLLAMA ;;
  *) echo "variant must be quickstart or normalized" >&2; exit 2 ;;
esac
init_run ollama "$VARIANT" "$2"
meta model "$MODEL"
meta model_source "ollama.com/library/$MODEL"
start_container

phase prereq "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates"

act "curl -fsSL https://ollama.com/install.sh | sh"
if ! phase install "curl -fsSL https://ollama.com/install.sh | sh"; then
  if grep -q 'requires zstd' "$OUT/install.log"; then
    note "install.sh failed on a clean ubuntu:24.04: 'This version requires zstd for extraction'. The docs do not list zstd; only the error names it. Full Ubuntu desktop and server images usually ship zstd, minimal containers do not."
    act "apt-get install zstd (named only in the installer's error)"
    phase install_zstd "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq zstd"
    act "curl -fsSL https://ollama.com/install.sh | sh (again)"
    phase install_retry "curl -fsSL https://ollama.com/install.sh | sh"
  else
    finish; exit 1
  fi
fi
note "Run as root in the container; on a desktop install.sh asks for the sudo password once (one more prompt)."
docker exec "$C" uname -r | grep -qi microsoft &&
  note "The container shares abbey1's WSL2 kernel, so install.sh takes its WSL2 branch and exits before any service setup."
meta version "$(docker exec "$C" ollama -v 2>&1 | tail -1)"

if docker exec "$C" curl -sf http://127.0.0.1:11434/api/version >/dev/null 2>&1; then
  note "install.sh left a running server; no 'ollama serve' needed."
else
  grep -q 'API is now available' "$OUT"/install*.log &&
    note "install.sh printed 'The Ollama API is now available at 127.0.0.1:11434' (an exit trap) but no server was running; 'ollama serve' was needed (docs: start it 'if it is not already running')."
  act "ollama serve (installer did not start a server: no systemd)"
  docker exec -d "$C" bash -lc 'ollama serve >/root/ollama-serve.log 2>&1'
  wait_for serve 60 "curl -sf http://127.0.0.1:11434/api/version"
fi

# The docs' one command is 'ollama run MODEL' (download, load, chat). Split into
# pull, load and the first prompt so each phase gets its own time.
act "ollama run $MODEL"
act "type the prompt"
phase download "ollama pull $MODEL"
phase load "curl -sf http://127.0.0.1:11434/api/generate -d '{\"model\":\"$MODEL\",\"keep_alive\":\"30m\"}'"
first_answer "http://127.0.0.1:11434/v1/chat/completions" "$MODEL"

docker exec "$C" ollama ps >"$OUT/ollama-ps.txt" 2>&1 || true
docker exec "$C" curl -s http://127.0.0.1:11434/api/ps >"$OUT/api-ps.json" || true
docker exec "$C" curl -s http://127.0.0.1:11434/api/show -d "{\"model\":\"$MODEL\"}" >"$OUT/api-show.json" || true
docker exec "$C" curl -s http://127.0.0.1:11434/api/tags >"$OUT/api-tags.json" || true
docker cp "$C:/root/ollama-serve.log" "$OUT/serve.log" >/dev/null 2>&1 || true
meta default_context "$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["models"][0].get("context_length",""))' "$OUT/api-ps.json" 2>/dev/null)"
meta context_source "/api/ps context_length (also ollama ps CONTEXT)"
meta model_file_bytes "$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["models"][0]["size"])' "$OUT/api-tags.json" 2>/dev/null)"
cat "$OUT/ollama-ps.txt" >&2
finish
