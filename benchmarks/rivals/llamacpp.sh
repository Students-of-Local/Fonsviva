#!/usr/bin/env bash
# llama.cpp on Linux, two official paths:
#   release: prebuilt ubuntu x64 CPU tarball from GitHub releases, then llama-server -hf
#   app:     the README's quick start today: llama.app/install.sh, then llama serve -hf
# Usage: llamacpp.sh release|app RUN [RELEASE_TAG]
source "$(dirname "$0")/lib.sh"
VARIANT=$1
init_run llamacpp "$VARIANT" "$2"
meta model "$NORM_HF ($NORM_HF_FILE, the -hf default quant)"
meta model_source "huggingface.co/$NORM_HF"
start_container

phase prereq "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates"

case "$VARIANT" in
  release)
    TAG=${3:-$(curl -fsSL 'https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10' |
      python3 -I -c 'import json,sys,re
for r in json.load(sys.stdin):
    if any(re.fullmatch(r"llama-b\d+-bin-ubuntu-x64\.tar\.gz", a["name"]) for a in r["assets"]):
        print(r["tag_name"]); break')}
    URL="https://github.com/ggml-org/llama.cpp/releases/download/$TAG/llama-$TAG-bin-ubuntu-x64.tar.gz"
    meta version "$TAG"
    act "open github.com/ggml-org/llama.cpp/releases and copy the ubuntu-x64 CPU asset link from the newest b* build"
    note "GitHub marks v1.0.0 as the Latest release, and it carries no binaries (only nightly-tag.txt). The CPU tarball is on the b* builds, one of about 20 Linux assets."
    act "curl -LO $URL"
    phase install "curl -fL -o /root/llama-$TAG-bin-ubuntu-x64.tar.gz $URL"
    act "tar xzf llama-$TAG-bin-ubuntu-x64.tar.gz"
    phase install_extract "cd /root && tar xzf llama-$TAG-bin-ubuntu-x64.tar.gz"
    BIN=$(docker exec "$C" bash -lc 'dirname "$(find /root -name llama-server -type f | head -1)"')
    act "cd into the extracted folder and run ./llama-server -hf $NORM_HF"
    SERVE="cd $BIN && ./llama-server -hf $NORM_HF"
    ;;
  app)
    act "curl -LsSf https://llama.app/install.sh | sh"
    phase install "curl -LsSf https://llama.app/install.sh | sh"
    meta version "$(docker exec "$C" bash -lc '~/.local/bin/llama version 2>&1 | head -1')"
    if ! docker exec "$C" bash -lc 'command -v llama' >/dev/null 2>&1; then
      note "README's 'llama serve' gives 'command not found' right after install: ~/.local/bin is not on PATH. The installer says to add it to the shell profile and open a new terminal (2 more actions), or to run ~/.llama-app/llama serve now; the count uses that shorter path."
    fi
    act "~/.llama-app/llama serve -hf $NORM_HF (the installer's no-PATH-change command)"
    SERVE="~/.llama-app/llama serve -hf $NORM_HF"
    ;;
  *) echo "variant must be release or app" >&2; exit 2 ;;
esac

# Start the server. If it dies on a missing shared library, do what a user
# would: install the package the error names, then start it again.
r0=$(rx "$C")
t0=$(now)
for _ in 1 2 3 4; do
  WAIT_T0=$(now)
  WAIT_R0=$(rx "$C")
  docker exec -d "$C" bash -lc "$SERVE >/root/server.log 2>&1"
  sleep 2
  LIB=$(docker exec "$C" bash -lc "grep -oE 'error while loading shared libraries: [^:]+' /root/server.log | awk '{print \$NF}'" || true)
  [ -z "$LIB" ] && break
  PKG=$(case "$LIB" in libgomp.so.1) echo libgomp1 ;; libcurl.so.4) echo libcurl4t64 ;; libssl.so.3) echo libssl3t64 ;; libvulkan.so.1) echo libvulkan1 ;; *) echo "" ;; esac)
  note "Server failed on a clean ubuntu:24.04: 'error while loading shared libraries: $LIB'. Installed $PKG; the docs do not list it. Full Ubuntu installs usually have it."
  act "apt-get install $PKG (named only by the loader error)"
  act "start the server again"
  [ -n "$PKG" ] || { finish; exit 1; }
  docker exec "$C" bash -lc "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $PKG" >>"$OUT/install_libs.log" 2>&1
done
[ "$WAIT_T0" != "$t0" ] && record install_libs "$t0" "$WAIT_T0" $((WAIT_R0 - r0)) 0
export WAIT_T0 WAIT_R0
# Download is done when the GGUF exists with no in-progress marker beside it.
wait_for download 900 'pgrep -f "llama(-server| serve)" >/dev/null || exit 0; f=$(find -L /root/.cache -name "*.gguf" -size +100M 2>/dev/null | head -1); [ -n "$f" ] && [ -z "$(find /root/.cache -name "*.downloadInProgress" -o -name "*.incomplete" -o -name "*.part" 2>/dev/null | head -1)" ]'
# The server prints its port once the model is loaded; read it from the log.
wait_for load 600 'p=$(grep -oE "listening on http://[0-9.]+:[0-9]+" /root/server.log | tail -1 | grep -oE "[0-9]+$"); [ -n "$p" ] && curl -sf http://127.0.0.1:$p/health'
PORT=$(docker exec "$C" bash -lc 'grep -oE "listening on http://[0-9.]+:[0-9]+" /root/server.log | tail -1 | grep -oE "[0-9]+$"')
meta port "$PORT"
[ "$PORT" = 8080 ] || note "The server listened on port $PORT, not 8080."
act "send the prompt (open http://localhost:$PORT, or curl /v1/chat/completions)"
first_answer "http://127.0.0.1:$PORT/v1/chat/completions" gemma

docker exec "$C" curl -s "http://127.0.0.1:$PORT/props" >"$OUT/props.json" || true
docker exec "$C" curl -s "http://127.0.0.1:$PORT/v1/models" >"$OUT/models.json" || true
docker cp "$C:/root/server.log" "$OUT/server.log" >/dev/null 2>&1 || true
docker exec "$C" bash -lc 'find -L /root/.cache -name "*.gguf" -printf "%s %p\n"' >"$OUT/gguf.txt" 2>&1 || true
meta default_context "$(python3 -I -c 'import json,sys; p=json.load(open(sys.argv[1])); print(p.get("default_generation_settings",{}).get("n_ctx") or p.get("n_ctx",""))' "$OUT/props.json" 2>/dev/null)"
meta context_source "/props default_generation_settings.n_ctx"
meta model_file_bytes "$(awk 'NR==1{print $1}' "$OUT/gguf.txt")"
finish
