#!/usr/bin/env bash
# Open WebUI bundled with Ollama (ghcr.io/open-webui/open-webui:ollama), CPU only.
# Runs on the docker host itself (Docker is the documented prerequisite), with
# no published port: the API is reached from inside the container's network.
# The UI steps it cannot click are scripted through the same API and counted as actions.
# Usage: openwebui.sh RUN
source "$(dirname "$0")/lib.sh"
init_run openwebui bundled-ollama "$1"
IMG=ghcr.io/open-webui/open-webui:ollama
MODEL=$NORM_OLLAMA
meta model "$MODEL"
meta model_source "ollama.com/library/$MODEL through Open WebUI's bundled Ollama"
note "Docker is a prerequisite and was already on the host; installing Docker is not counted."
note "Ran without '-p 3000:8080' so nothing listened on abbey1's network; the API was reached inside the container."

cleanup() {
  docker rm -f "$C" >/dev/null 2>&1 || true
  docker volume rm -f "$C-data" "$C-ollama" >/dev/null 2>&1 || true
  docker rmi "$IMG" >/dev/null 2>&1 || true
}
# Fresh pull every run.
docker rmi "$IMG" >/dev/null 2>&1 || true

act "docker run -d -p 3000:8080 -v ollama:/root/.ollama -v open-webui:/app/backend/data --name open-webui --restart always $IMG"
t0=$(now)
rc=0
docker pull "$IMG" >"$OUT/install.log" 2>&1 || rc=$?
t1=$(now)
record install "$t0" "$t1" "$(python3 -I "$HERE/image_bytes.py" ghcr.io open-webui/open-webui ollama)" "$rc"
meta image_digest "$(docker image inspect -f '{{index .RepoDigests 0}}' "$IMG")"

t0=$(now)
docker run -d --name "$C" --cpus "$CPUS" --memory "$MEM" -v "$C-ollama:/root/.ollama" -v "$C-data:/app/backend/data" "$IMG" >/dev/null
rc=1
for _ in $(seq 1 2400); do
  if docker exec "$C" curl -sf http://127.0.0.1:8080/health >/dev/null 2>&1; then rc=0; break; fi
  sleep 0.25
done
t1=$(now)
record serve "$t0" "$t1" "$(rx "$C")" "$rc"
docker logs "$C" >"$OUT/serve.log" 2>&1 || true
meta version "$(docker exec "$C" curl -s http://127.0.0.1:8080/api/version)"

act "open http://localhost:3000 in a browser"
act "create the admin account (name, email, password)"
PASS=$(head -c 18 /dev/urandom | base64 | tr -d '/+=')
docker exec "$C" curl -s http://127.0.0.1:8080/api/v1/auths/signup -H 'Content-Type: application/json' \
  -d "{\"name\":\"bench\",\"email\":\"bench@example.invalid\",\"password\":\"$PASS\"}" >"$OUT/signup.json"
TOKEN=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$OUT/signup.json")
sed -i 's/"token": *"[^"]*"/"token":"<redacted>"/' "$OUT/signup.json"

act "Admin Panel > Settings > Connections"
act "click Manage on the Ollama connection"
act "type $MODEL and click download"
phase download "curl -sfN -H 'Authorization: Bearer $TOKEN' -H 'Content-Type: application/json' http://127.0.0.1:8080/ollama/api/pull -d '{\"name\":\"$MODEL\"}' | tail -3 | tee /dev/stderr | grep -q success"
act "pick $MODEL in the chat's model selector"
act "type the prompt"
phase load "curl -sf -H 'Authorization: Bearer $TOKEN' -H 'Content-Type: application/json' http://127.0.0.1:8080/ollama/api/generate -d '{\"model\":\"$MODEL\",\"keep_alive\":\"30m\"}'"
first_answer "http://127.0.0.1:8080/api/chat/completions" "$MODEL" "$TOKEN"

docker exec "$C" curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/ollama/api/ps >"$OUT/api-ps.json" || true
docker exec "$C" curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/ollama/api/tags >"$OUT/api-tags.json" || true
docker exec "$C" ollama ps >"$OUT/ollama-ps.txt" 2>&1 || true
meta default_context "$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["models"][0].get("context_length",""))' "$OUT/api-ps.json" 2>/dev/null)"
meta context_source "bundled Ollama /api/ps context_length (Open WebUI sends no num_ctx by default)"
meta model_file_bytes "$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["models"][0]["size"])' "$OUT/api-tags.json" 2>/dev/null)"
cat "$OUT/ollama-ps.txt" >&2
finish
