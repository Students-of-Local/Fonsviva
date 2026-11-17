#!/usr/bin/env bash
# Network baseline: download a fixed 100 MB file from a CDN in a fresh container, twice.
# Usage: netspeed.sh
source "$(dirname "$0")/lib.sh"
URL=https://cachefly.cachefly.net/100mb.test
OUT="$HERE/logs/netspeed"
C=fv-rival-netspeed
wait_quiet_host
rm -rf "$OUT"
mkdir -p "$OUT"
cut -d' ' -f1-3 /proc/loadavg >"$OUT/load_start.txt"
trap 'docker rm -f "$C" >/dev/null 2>&1 || true' EXIT
docker rm -f "$C" >/dev/null 2>&1 || true
docker run -d --name "$C" "$IMAGE" sleep infinity >/dev/null
docker exec "$C" bash -c "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates" >/dev/null 2>&1
for i in 1 2 3; do
  docker exec "$C" curl -s -o /dev/null -w '{"url":"%{url_effective}","bytes":%{size_download},"seconds":%{time_total},"bytes_per_s":%{speed_download}}\n' "$URL"
done | tee "$OUT/samples.jsonl"
cut -d' ' -f1-3 /proc/loadavg >"$OUT/load_end.txt"
python3 -I - "$OUT" <<'EOF'
import json, statistics, sys, datetime
out = sys.argv[1]
s = [json.loads(l) for l in open(out + "/samples.jsonl") if l.strip()]
bps = statistics.median(x["bytes_per_s"] for x in s)
r = {"date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
     "url": s[0]["url"], "file_bytes": s[0]["bytes"], "samples": s,
     "median_seconds": statistics.median(x["seconds"] for x in s),
     "median_MB_per_s": round(bps / 1e6, 1), "median_Mbit_per_s": round(bps * 8 / 1e6),
     "host_load_start": open(out + "/load_start.txt").read().strip(),
     "host_load_end": open(out + "/load_end.txt").read().strip()}
json.dump(r, open(out + "/result.json", "w"), indent=2)
print(json.dumps({k: r[k] for k in ("median_seconds", "median_MB_per_s", "median_Mbit_per_s")}))
EOF
