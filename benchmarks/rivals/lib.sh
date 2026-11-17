# Shared helpers for the rival scripts. Source it; do not run it.
# Runs on the docker host (abbey1). Every container is named fv-rival-*.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
IMAGE=ubuntu:24.04
PY_IMAGE=python:3.13-alpine
PROMPT="Name three primary colors."
MAX_TOKENS=64
NORM_OLLAMA=gemma3:1b
NORM_HF=ggml-org/gemma-3-1b-it-GGUF
NORM_HF_FILE=gemma-3-1b-it-Q4_K_M.gguf
# No single step may hang a run (a hidden prompt, a stuck download).
PHASE_TIMEOUT=1500
# Same limits for every rival so runs compare.
CPUS=8
MEM=12g

now() { date +%s.%N; }

# Hold while the host is busy: a loaded machine skews every time and speed figure.
MAX_LOAD=10
wait_quiet_host() {
  while awk -v max="$MAX_LOAD" '{ exit !($1 > max) }' /proc/loadavg; do
    echo "holding: load $(cut -d' ' -f1-3 /proc/loadavg) is over $MAX_LOAD" >&2
    sleep 15
  done
}

# init_run RIVAL VARIANT RUN
init_run() {
  wait_quiet_host
  RIVAL=$1 VARIANT=$2 RUN=$3
  C="fv-rival-$RIVAL-$VARIANT-$RUN"
  OUT="$HERE/logs/$RIVAL-$VARIANT/run$RUN"
  rm -rf "$OUT"
  mkdir -p "$OUT"
  : >"$OUT/phases.tsv"
  : >"$OUT/actions.txt"
  : >"$OUT/notes.txt"
  : >"$OUT/meta.tsv"
  meta rival "$RIVAL"
  meta variant "$VARIANT"
  meta run "$RUN"
  meta date "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  meta host "$(hostname) $(nproc) cores, container --cpus $CPUS --memory $MEM"
  meta host_load_start "$(cut -d' ' -f1-3 /proc/loadavg)"
  trap cleanup EXIT
}

cleanup() {
  docker rm -f "$C" >/dev/null 2>&1 || true
}

meta() { printf '%s\t%s\n' "$1" "$2" >>"$OUT/meta.tsv"; }
note() { printf '%s\n' "$*" >>"$OUT/notes.txt"; echo "NOTE: $*" >&2; }
# act TEXT: one user action from the official docs (a command typed, a prompt
# answered, a file edited or a UI step).
act() { printf '%s\n' "$*" >>"$OUT/actions.txt"; }

start_container() {
  docker rm -f "$C" >/dev/null 2>&1 || true
  docker run -d --name "$C" --cpus "$CPUS" --memory "$MEM" "$@" "$IMAGE" sleep infinity >/dev/null
}

rx() { docker exec "$1" cat /sys/class/net/eth0/statistics/rx_bytes; }

# record NAME T0 T1 BYTES RC
record() {
  printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >>"$OUT/phases.tsv"
  awk -v n="$1" -v a="$2" -v b="$3" -v by="$4" -v rc="$5" 'BEGIN { printf "%-14s %8.2fs %12d bytes rc=%s\n", n, b - a, by, rc }' >&2
}

# phase NAME CMD...: run CMD in the container with bash -lc, log it, record time and bytes.
phase() {
  local name=$1 r0 t0 t1 r1 rc=0
  shift
  r0=$(rx "$C")
  t0=$(now)
  timeout "$PHASE_TIMEOUT" docker exec "$C" bash -lc "$*" >"$OUT/$name.log" 2>&1 </dev/null || rc=$?
  t1=$(now)
  r1=$(rx "$C")
  record "$name" "$t0" "$t1" $((r1 - r0)) "$rc"
  return "$rc"
}

# wait_for NAME TIMEOUT CMD...: poll CMD in the container until it succeeds; record the wait.
# Set WAIT_T0 and WAIT_R0 to start the phase earlier (when its work really began).
wait_for() {
  local name=$1 timeout=$2 r0 t0 t1 r1 rc=1
  shift 2
  r0=${WAIT_R0:-$(rx "$C")}
  t0=${WAIT_T0:-$(now)}
  unset WAIT_T0 WAIT_R0
  local deadline=$(($(date +%s) + timeout))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if docker exec "$C" bash -lc "$*" >/dev/null 2>&1; then rc=0; break; fi
    sleep 0.25
  done
  t1=$(now)
  r1=$(rx "$C")
  record "$name" "$t0" "$t1" $((r1 - r0)) "$rc"
  return "$rc"
}

# first_answer URL MODEL [TOKEN] [NETNS_CONTAINER]: stream the prompt from a helper
# container that shares the rival's network namespace, so nothing is added to it.
first_answer() {
  local url=$1 model=$2 token=${3:-} ns=${4:-$C}
  local r0 t0 t1 r1 rc=0
  r0=$(rx "$ns")
  t0=$(now)
  docker run --rm --network "container:$ns" -v "$HERE/ttft.py:/ttft.py:ro" "$PY_IMAGE" \
    python -I /ttft.py "$url" "$model" "$PROMPT" "$MAX_TOKENS" $token >"$OUT/ttft.json" 2>"$OUT/ttft.err" || rc=$?
  t1=$(now)
  r1=$(rx "$ns")
  cat "$OUT/ttft.json" >&2
  # The phase starts when the request is sent, not when the helper container starts.
  local t_send
  t_send=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["t_send"])' "$OUT/ttft.json" 2>/dev/null || echo "$t0")
  record first_answer "$t_send" "$t1" $((r1 - r0)) "$rc"
  # A thinking model can spend the whole 64-token budget reasoning. Ask again
  # without the cap to time the first visible answer.
  if python3 -I -c 'import json,sys; j=json.load(open(sys.argv[1])); sys.exit(0 if j.get("ok") and not j.get("answered") else 1)' "$OUT/ttft.json" 2>/dev/null; then
    note "No visible answer within $MAX_TOKENS tokens: the model spent them reasoning. Asked again with max_tokens 2048 to time the first visible answer (ttft_uncapped.json)."
    docker run --rm --network "container:$ns" -v "$HERE/ttft.py:/ttft.py:ro" "$PY_IMAGE" \
      python -I /ttft.py "$url" "$model" "$PROMPT" 2048 $token >"$OUT/ttft_uncapped.json" 2>>"$OUT/ttft.err" || true
    cat "$OUT/ttft_uncapped.json" >&2
  fi
}

# finish: write result.json for this run.
finish() {
  meta host_load_end "$(cut -d' ' -f1-3 /proc/loadavg)"
  python3 -I "$HERE/emit.py" "$OUT"
}
