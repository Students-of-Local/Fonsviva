#!/bin/sh
# Runs install.sh against release dirs made from a fonsviva binary, with HOME
# in a scratch dir and uname, systemctl, launchctl and loginctl replaced by
# shims that log their argv. The shims start the real `fonsviva org serve`
# from the service file they were given. Both the systemd and the launchd
# flavor run on any host. Usage: install-test.sh <fonsviva binary>
set -eu

[ $# -eq 1 ] || { echo "usage: install-test.sh <fonsviva binary>" >&2; exit 2; }
fv=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
root=$(cd "$(dirname "$0")/../.." && pwd)
installer="$root/install.sh"
S=$(mktemp -d)
S=$(cd "$S" && pwd)
pids="$S/pids"
: >"$pids"
cleanup() {
  while read -r p; do
    if kill -0 "$p" 2>/dev/null; then kill "$p"; fi
  done <"$pids"
  if [ -n "${stub_pid:-}" ] && kill -0 "$stub_pid" 2>/dev/null; then kill "$stub_pid"; fi
  rm -rf "$S"
}
trap cleanup EXIT INT TERM

unset XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME XDG_CACHE_HOME FONSVIVA_HOME
export TMPDIR="$S/tmp"
mkdir -p "$TMPDIR" "$S/shim" "$S/work" "$S/homes"
failures=0
fail() { printf 'FAIL %s: %s\n' "$case" "$*"; failures=$((failures + 1)); }
pass() { printf 'ok   %s\n' "$case"; }

if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | awk '{print $1}'; }
else
  sha() { shasum -a 256 "$1" | awk '{print $1}'; }
fi
mode() { if stat -c %a "$1" >/dev/null 2>&1; then stat -c %a "$1"; else stat -f %Lp "$1"; fi; }
inode() { if stat -c %i "$1" >/dev/null 2>&1; then stat -c %i "$1"; else stat -f %i "$1"; fi; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

real_uname=$(command -v uname)
cat >"$S/shim/uname" <<EOF
#!/bin/sh
case "\${1:-}" in
  -s) echo "\${FAKE_OS:-\$($real_uname -s)}" ;;
  -m) echo "\${FAKE_ARCH:-x86_64}" ;;
  *) exec $real_uname "\$@" ;;
esac
EOF

cat >"$S/shim/common.sh" <<'EOF'
stop_server() {
  if [ -f "$SHIM_DIR/server.pid" ]; then
    p=$(cat "$SHIM_DIR/server.pid")
    if kill -0 "$p" 2>/dev/null; then kill "$p"; fi
    while kill -0 "$p" 2>/dev/null; do sleep 0.1; done
    rm -f "$SHIM_DIR/server.pid"
  fi
}
start_server() {
  log=$1
  shift
  mkdir -p "$(dirname "$log")"
  if [ "${SHIM_DEAD:-0}" = 1 ]; then
    i=1
    while [ $i -le 24 ]; do echo "dead line $i" >>"$log"; i=$((i + 1)); done
    return 0
  fi
  "$@" >>"$log" 2>&1 </dev/null &
  echo $! >"$SHIM_DIR/server.pid"
  echo $! >>"$SHIM_PIDS"
}
EOF

cat >"$S/shim/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >>"$SHIM_LOG"
if [ "${SHIM_EAT:-0}" = 1 ]; then cat >/dev/null; fi
. "$SHIM_DIR/common.sh"
[ "$1" = --user ] || exit 1
case ${SHIM_MGR_XDG:-} in
  /*) unit="$SHIM_MGR_XDG/systemd/user/fonsviva-org.service" ;;
  *) unit="$HOME/.config/systemd/user/fonsviva-org.service" ;;
esac
case $2 in
  daemon-reload | enable) ;;
  show-environment)
    if [ -n "${SHIM_MGR_RAW:-}" ]; then echo "$SHIM_MGR_RAW"
    elif [ -n "${SHIM_MGR_XDG:-}" ]; then echo "XDG_CONFIG_HOME=$SHIM_MGR_XDG"
    fi
    ;;
  restart)
    stop_server
    log=$(sed -n 's/^StandardError=append://p' "$unit")
    # shellcheck disable=SC2046
    start_server "$log" $(sed -n 's/^ExecStart=//p' "$unit")
    ;;
  disable) [ "$3" = --now ] && stop_server ;;
  *) exit 1 ;;
esac
EOF

cat >"$S/shim/launchctl" <<'EOF'
#!/bin/sh
echo "launchctl $*" >>"$SHIM_LOG"
. "$SHIM_DIR/common.sh"
case $1 in
  print)
    case $2 in
      */*/*) ;;
      *) [ "${SHIM_NOGUI:-0}" = 1 ] && exit 113; exit 0 ;;
    esac
    if [ -f "$SHIM_DIR/unloading" ]; then
      left=$(cat "$SHIM_DIR/unloading")
      if [ "$left" -gt 0 ]; then echo $((left - 1)) >"$SHIM_DIR/unloading"; exit 0; fi
      rm -f "$SHIM_DIR/unloading"
    fi
    [ -f "$SHIM_DIR/loaded" ] || exit 113
    ;;
  bootout)
    stop_server
    rm -f "$SHIM_DIR/loaded"
    if [ "${SHIM_SLOW_BOOTOUT:-0}" = 1 ]; then echo 3 >"$SHIM_DIR/unloading"; fi
    ;;
  bootstrap)
    if [ -f "$SHIM_DIR/unloading" ]; then echo "Bootstrap failed: 5: Input/output error" >&2; exit 5; fi
    plist=$3
    touch "$SHIM_DIR/loaded"
    log=$(sed -n '/<key>StandardErrorPath<\/key>/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}' "$plist")
    # shellcheck disable=SC2046
    start_server "$log" $(sed -n '/<key>ProgramArguments<\/key>/,/<\/array>/s/.*<string>\(.*\)<\/string>.*/\1/p' "$plist")
    ;;
  *) exit 1 ;;
esac
EOF

cat >"$S/shim/loginctl" <<'EOF'
#!/bin/sh
echo "loginctl $*" >>"$SHIM_LOG"
echo "Linger=${SHIM_LINGER:-yes}"
EOF
chmod 0755 "$S/shim/"*
printf '#!/usr/bin/env python3\nimport os, sys\nos.setsid()\nos.execvp(sys.argv[1], sys.argv[1:])\n' >"$S/notty"
chmod 0755 "$S/notty"

tag="v$("$fv" --version | awk '{print $2}')"
rel() {
  dir=$1 t=$2 target=$3 binary=$4
  mkdir -p "$dir/stage"
  cp "$binary" "$dir/stage/fonsviva"
  chmod 0755 "$dir/stage/fonsviva"
  tar -C "$dir/stage" -czf "$dir/fonsviva-$t-$target.tar.gz" fonsviva
  rm -rf "$dir/stage"
  echo "$(sha "$dir/fonsviva-$t-$target.tar.gz")  fonsviva-$t-$target.tar.gz" >>"$dir/SHA256SUMS"
}
printf '#!/bin/sh\nexit 1\n' >"$S/broken"
cat >"$S/broken-writer" <<'EOF'
#!/bin/sh
mkdir -p "$HOME/.local/share/other" && touch "$HOME/.local/share/other/keep"
exit 1
EOF
for os in Linux Darwin; do
  [ $os = Linux ] && target=x86_64-unknown-linux-gnu || target=x86_64-apple-darwin
  rel "$S/rel-$os" "$tag" "$target" "$fv"
  rel "$S/rel-$os" v0.0.0 "$target" "$fv"
done
rel "$S/rel-broken" "$tag" x86_64-unknown-linux-gnu "$S/broken"
rel "$S/rel-broken-writer" "$tag" x86_64-unknown-linux-gnu "$S/broken-writer"
mkdir -p "$S/rel-mismatch" "$S/rel-noline"
cp "$S/rel-Linux/fonsviva-$tag-x86_64-unknown-linux-gnu.tar.gz" "$S/rel-mismatch/"
cp "$S/rel-Linux/fonsviva-$tag-x86_64-unknown-linux-gnu.tar.gz" "$S/rel-noline/"
echo "$(printf '0%.0s' $(seq 64))  fonsviva-$tag-x86_64-unknown-linux-gnu.tar.gz" >"$S/rel-mismatch/SHA256SUMS"
awk '!/x86_64-unknown-linux-gnu/' "$S/rel-Linux/SHA256SUMS" "$S/rel-Darwin/SHA256SUMS" >"$S/rel-noline/SHA256SUMS"

n=0
fresh() {
  n=$((n + 1))
  H="$S/homes/h$n"
  mkdir -p "$H"
  SHIM_DIR="$S/shimstate/h$n"
  mkdir -p "$SHIM_DIR"
  cp "$S/shim/common.sh" "$SHIM_DIR/"
  SHIM_LOG="$SHIM_DIR/calls"
  : >"$SHIM_LOG"
  out="$S/work/out$n"
  err="$S/work/err$n"
}
EXTRA="" LAUNCH="" PIPE=""
install() {
  if [ -n "$PIPE" ]; then
    set -- -s -- "$@"
  else
    set -- "$installer" "$@"
  fi
  # shellcheck disable=SC2086
  $LAUNCH env HOME="$H" USER=tester PATH="$S/shim:$PATH" FAKE_OS="$OS" \
    SHIM_DIR="$SHIM_DIR" SHIM_LOG="$SHIM_LOG" SHIM_PIDS="$pids" \
    FONSVIVA_SYSTEMCTL="$S/shim/systemctl" FONSVIVA_LAUNCHCTL="$S/shim/launchctl" \
    FONSVIVA_LOGINCTL="$S/shim/loginctl" FONSVIVA_RELEASE_BASE="${REL:-file://$S/rel-$OS}" \
    $EXTRA sh "$@" >"$out" 2>"$err" <"${PIPE:-/dev/null}"
}
run() { set +e; install "$@"; code=$?; set -e; EXTRA="" LAUNCH="" REL="" PIPE=""; }
touched() { grep -v ' show-environment$' "$SHIM_LOG" | grep -q .; }

service_file() {
  if [ "$OS" = Linux ]; then
    echo "$H/.config/systemd/user/fonsviva-org.service"
  else
    echo "$H/Library/LaunchAgents/com.studentsoflocal.fonsviva-org.plist"
  fi
}
logfile() {
  if [ "$OS" = Linux ]; then echo "$H/.local/state/fonsviva/org.log"; else echo "$H/Library/Logs/fonsviva-org.log"; fi
}
expected_service() {
  b="$H/.local/bin/fonsviva" d="$H/.local/share/fonsviva/org" l=$(logfile)
  if [ "$OS" = Linux ]; then
    cat <<EOF
[Unit]
Description=Fonsviva org server
After=network.target

[Service]
ExecStart=$b org serve --data $d --listen 127.0.0.1:$port
Restart=always
StandardOutput=append:$l
StandardError=append:$l

[Install]
WantedBy=default.target
EOF
  else
    cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.studentsoflocal.fonsviva-org</string>
  <key>ProgramArguments</key>
  <array>
    <string>$b</string>
    <string>org</string>
    <string>serve</string>
    <string>--data</string>
    <string>$d</string>
    <string>--listen</string>
    <string>127.0.0.1:$port</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$l</string>
  <key>StandardErrorPath</key>
  <string>$l</string>
</dict>
</plist>
EOF
  fi
}
org_url=https://fonsviva.example.org
link_re='^https://fonsviva\.example\.org/join/[0-9a-f]\{64\}$'
tail3() {
  printf 'Fonsviva %s is running at http://127.0.0.1:%s for %s.\nSign in as the first admin (the link works once, for 24 hours):\n' "$tag" "$port" "$org_url"
}
api() {
  method=$1 path=$2 cookie=$3 body=$4
  set -- -sS -o "$S/work/body" -D "$S/work/head" -w '%{http_code}' -X "$method" \
    -H 'X-Fonsviva-Request: 1' -H 'Content-Type: application/json'
  [ -n "$cookie" ] && set -- "$@" -H "Cookie: $cookie"
  [ -n "$body" ] && set -- "$@" --data "$body"
  curl "$@" "http://127.0.0.1:$port$path" || :
}

stub_port=$(free_port)
python3 - "$stub_port" >"$S/work/stub.log" 2>&1 <<'EOF' &
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def answer(self, body):
        b = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def do_GET(self):
        self.answer({"object": "list", "data": [{"id": "tiny-1", "object": "model", "owned_by": "stub"}]})
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.answer({"id": "chatcmpl-stub", "object": "chat.completion",
                     "choices": [{"index": 0, "message": {"role": "assistant", "content": "stub reply from the install test"}, "finish_reason": "stop"}],
                     "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}})
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
stub_pid=$!

REL=""
for OS in Linux Darwin; do
  port=$(free_port)
  case="1 first install ($OS)"
  fresh
  [ $OS = Linux ] && EXTRA=SHIM_LINGER=no
  run --url "$org_url" --port "$port"
  b="$H/.local/bin/fonsviva" d="$H/.local/share/fonsviva/org"
  link1=$(tail -n 1 "$out")
  if [ "$code" -ne 0 ]; then fail "exit $code: $(cat "$err")"
  elif ! cmp -s "$b" "$fv" || [ "$(mode "$b")" != 755 ]; then fail "binary at $b"
  elif [ "$(mode "$d")" != 700 ]; then fail "data dir mode $(mode "$d")"
  elif [ -n "$(find "$d" -type f ! -perm 600)" ]; then fail "store files not 0600: $(find "$d" -type f ! -perm 600)"
  elif [ "$(expected_service)" != "$(cat "$(service_file)")" ]; then fail "service file differs: $(cat "$(service_file)")"
  elif [ $OS = Linux ] && [ "$(cat "$SHIM_LOG")" != "$(printf 'systemctl --user show-environment\nsystemctl --user daemon-reload\nsystemctl --user enable fonsviva-org.service\nsystemctl --user restart fonsviva-org.service\nloginctl show-user tester -p Linger')" ]; then fail "service calls: $(cat "$SHIM_LOG")"
  elif [ $OS = Darwin ] && [ "$(cat "$SHIM_LOG")" != "$(printf 'launchctl print gui/%s\nlaunchctl print gui/%s/com.studentsoflocal.fonsviva-org\nlaunchctl bootstrap gui/%s %s' "$(id -u)" "$(id -u)" "$(id -u)" "$(service_file)")" ]; then fail "service calls: $(cat "$SHIM_LOG")"
  elif [ "$(tail -n 3 "$out" | head -n 2)" != "$(tail3)" ] || ! echo "$link1" | grep -q "$link_re"; then fail "success lines: $(cat "$out")"
  elif [ "$(curl -fsS "http://127.0.0.1:$port/api/org/v1/health")" != '{"ok":true}' ]; then fail "health"
  else pass; fi
  if [ $OS = Linux ]; then
    case="10 Linger not yes (Linux)"
    if grep -qx "Warning: the org server stops when you log out. To keep it running, run: loginctl enable-linger tester" "$err" && [ "$code" -eq 0 ]; then pass; else fail "no linger warning: $(cat "$err")"; fi
  fi

  case="2 the link signs in an admin who chats with a stub source ($OS)"
  token1=${link1##*/}
  c2=$(api POST /api/org/v1/join "" "{\"token\":\"$token1\",\"name\":\"Ada\"}")
  cookie=$(sed -n 's/^[Ss]et-[Cc]ookie: \(fv_session=[0-9a-f]*\).*/\1/p' "$S/work/head" | tr -d '\r')
  role=$(sed -n 's/.*"role":"\([a-z]*\)".*/\1/p' "$S/work/body")
  if [ "$c2" != 200 ] || [ "$role" != admin ] || [ -z "$cookie" ]; then fail "join $c2: $(cat "$S/work/body")"
  else
    session_tokens="${session_tokens:-} ${cookie#fv_session=}"
    c=$(api POST /api/org/v1/sources "$cookie" "{\"name\":\"stub\",\"baseUrl\":\"http://127.0.0.1:$stub_port/v1\"}")
    if [ "$c" != 200 ]; then fail "add source $c: $(cat "$S/work/body")"
    else
      c=$(api POST /api/org/v1/chat/completions "$cookie" '{"model":"stub/tiny-1","messages":[{"role":"user","content":"hi"}]}')
      if [ "$c" = 200 ] && grep -q 'stub reply from the install test' "$S/work/body"; then pass; else fail "chat $c: $(cat "$S/work/body")"; fi
    fi
  fi

  case="5 second run keeps the store and makes a new link ($OS)"
  ino=$(inode "$b")
  : >"$SHIM_LOG"
  out="$S/work/out$n.again" err="$S/work/err$n.again"
  [ $OS = Darwin ] && EXTRA=SHIM_SLOW_BOOTOUT=1
  run --url "$org_url" --port "$port" --version "$tag"
  link2=$(tail -n 1 "$out")
  if [ "$code" -ne 0 ]; then fail "exit $code: $(cat "$err")"
  elif [ "$link2" = "$link1" ] || ! echo "$link2" | grep -q "$link_re"; then fail "link $link2"
  elif [ "$(inode "$b")" = "$ino" ] || ! cmp -s "$b" "$fv" || [ -n "$(find "$H/.local/bin" -name '.fonsviva*')" ]; then fail "binary not replaced in one rename"
  elif [ "$(api GET /api/org/v1/me "$cookie" "")" != 200 ]; then fail "the case 2 session is gone, so the store changed"
  elif [ "$(api POST /api/org/v1/join "" "{\"token\":\"$token1\",\"name\":\"Eve\"}")" != 410 ]; then fail "the first link joined twice"
  elif [ -s "$err" ]; then fail "unexpected warning: $(cat "$err")"
  elif [ $OS = Darwin ] && ! grep -qx "launchctl bootout gui/$(id -u)/com.studentsoflocal.fonsviva-org" "$SHIM_LOG"; then fail "agent not replaced: $(cat "$SHIM_LOG")"
  else pass; fi
  case="5 an older --version asks first, and --yes installs it ($OS)"
  ino=$(inode "$b")
  LAUNCH="$S/notty"
  run --url "$org_url" --port "$port" --version v0.0.0
  if [ "$code" -eq 0 ] || [ "$(inode "$b")" != "$ino" ] || ! grep -q -- "--yes" "$err"; then fail "downgrade without --yes: exit $code, $(cat "$err")"
  else
    run --url "$org_url" --port "$port" --version v0.0.0 --yes
    if [ "$code" -eq 0 ] && [ "$(inode "$b")" != "$ino" ]; then pass; else fail "downgrade with --yes: exit $code, $(cat "$err")"; fi
  fi

  case="8 uninstall keeps the store ($OS)"
  server=$(cat "$SHIM_DIR/server.pid")
  : >"$SHIM_LOG"
  run --uninstall --port "$port"
  if [ "$code" -ne 0 ]; then fail "exit $code: $(cat "$err")"
  elif [ -e "$b" ] || [ -e "$(service_file)" ]; then fail "binary or service left"
  elif [ ! -f "$d/org.db" ] || ! grep -qF "kept at $d." "$out"; then fail "store not kept and named: $(cat "$out")"
  elif kill -0 "$server" 2>/dev/null; then fail "server still running"
  elif [ $OS = Linux ] && ! grep -qx "systemctl --user disable --now fonsviva-org.service" "$SHIM_LOG"; then fail "calls: $(cat "$SHIM_LOG")"
  elif [ $OS = Darwin ] && ! grep -qx "launchctl bootout gui/$(id -u)/com.studentsoflocal.fonsviva-org" "$SHIM_LOG"; then fail "calls: $(cat "$SHIM_LOG")"
  else pass; fi

  case="6 --no-service ($OS)"
  fresh
  run --url "$org_url" --no-service
  if [ "$code" -ne 0 ]; then fail "exit $code: $(cat "$err")"
  elif [ -e "$(service_file)" ] || [ -s "$SHIM_LOG" ]; then fail "service touched: $(cat "$SHIM_LOG")"
  elif [ ! -f "$H/.local/share/fonsviva/org/org.db" ] || ! tail -n 1 "$out" | grep -q "$link_re"; then fail "no store or link: $(cat "$out")"
  else pass; fi

  case="7 a server that never answers health ($OS)"
  fresh
  port=$(free_port)
  start=$(date +%s)
  EXTRA=SHIM_DEAD=1
  run --url "$org_url" --port "$port"
  took=$(($(date +%s) - start))
  if [ "$code" -eq 0 ]; then fail "exit 0"
  elif [ "$took" -lt 19 ] || [ "$took" -gt 45 ]; then fail "gave up after ${took}s"
  elif ! grep -qx "dead line 5" "$err" || ! grep -qx "dead line 24" "$err" || grep -qx "dead line 4" "$err"; then fail "not the last 20 log lines: $(cat "$err")"
  elif ! grep -qx "Read the rest with: cat $(logfile)" "$err"; then fail "no command for the rest: $(cat "$err")"
  elif ! cmp -s "$H/.local/bin/fonsviva" "$fv" || [ ! -f "$H/.local/share/fonsviva/org/org.db" ]; then fail "binary or store gone"
  else pass; fi
done

OS=Linux
case="3 a tarball that doesn't match SHA256SUMS"
fresh
REL="file://$S/rel-mismatch"
run --url "$org_url" --version "$tag"
if [ "$code" -ne 0 ] && [ ! -e "$H/.local" ] && ! touched && grep -q "does not match SHA256SUMS" "$err"; then pass; else fail "exit $code, $(cat "$err")"; fi
case="3 SHA256SUMS with no line for the tarball"
fresh
REL="file://$S/rel-noline"
run --url "$org_url" --version "$tag"
if [ "$code" -ne 0 ] && [ ! -e "$H/.local" ] && ! touched && grep -q "no line for" "$err"; then pass; else fail "exit $code, $(cat "$err")"; fi
case="3 SHA256SUMS with no line for the latest tarball"
REL="file://$S/rel-noline"
run --url "$org_url"
if [ "$code" -ne 0 ] && [ ! -e "$H/.local" ] && ! touched && grep -q "no line for" "$err"; then pass; else fail "latest: exit $code, $(cat "$err")"; fi

case="4 a binary that fails --version"
fresh
run --url "$org_url" --no-service
ino=$(inode "$H/.local/bin/fonsviva")
REL="file://$S/rel-broken"
run --url "$org_url" --version "$tag"
if [ "$code" -ne 0 ] && [ "$(inode "$H/.local/bin/fonsviva")" = "$ino" ] && cmp -s "$H/.local/bin/fonsviva" "$fv" && ! touched && grep -q -- "--version failed" "$err" && [ -z "$(find "$H/.local/bin" -name '.fonsviva*')" ]; then pass; else fail "exit $code, $(cat "$err")"; fi

case="9 unknown uname -m"
fresh
EXTRA=FAKE_ARCH=sparc64
run --url "$org_url"
if [ "$code" -ne 0 ] && [ "$(wc -l <"$err")" -eq 1 ] && grep -q "sparc64" "$err" && [ ! -e "$H/.local" ]; then pass; else fail "exit $code, $(cat "$err")"; fi
case="9 no tar on PATH"
fresh
mkdir -p "$S/notar"
for t in awk sed cat cp mv rm mkdir chmod mktemp id curl sha256sum shasum tail sleep dirname env; do
  p=$(command -v "$t" 2>/dev/null) && ln -sf "$p" "$S/notar/$t"
done
set +e
env HOME="$H" PATH="$S/shim:$S/notar" FAKE_OS=Linux FONSVIVA_SYSTEMCTL="$S/shim/systemctl" \
  FONSVIVA_RELEASE_BASE="file://$S/rel-Linux" /bin/sh "$installer" --url "$org_url" >"$out" 2>"$err"
code=$?
set -e
if [ "$code" -ne 0 ] && [ "$(wc -l <"$err")" -eq 1 ] && grep -q "needs tar" "$err" && [ ! -e "$H/.local" ]; then pass; else fail "exit $code, $(cat "$err")"; fi

case="12 piped into sh, with a service manager that reads stdin"
fresh
port=$(free_port)
PIPE=$installer EXTRA=SHIM_EAT=1
run --url "$org_url" --port "$port"
if [ "$code" -eq 0 ] && tail -n 1 "$out" | grep -q "$link_re" && [ "$(curl -fsS "http://127.0.0.1:$port/api/org/v1/health")" = '{"ok":true}' ]; then pass; else fail "exit $code, out: $(cat "$out"), err: $(cat "$err")"; fi
run --uninstall --port "$port" >/dev/null

case="13 XDG_CONFIG_HOME set in the shell"
fresh
port=$(free_port)
EXTRA="XDG_CONFIG_HOME=$H/.cfg"
run --url "$org_url" --port "$port"
if [ "$code" -eq 0 ] && [ -f "$H/.config/systemd/user/fonsviva-org.service" ] && [ ! -e "$H/.cfg" ]; then pass; else fail "exit $code, $(cat "$err")"; fi
run --uninstall --port "$port" >/dev/null

case="14 a proxy that would eat the loopback health probe"
fresh
port=$(free_port)
EXTRA="http_proxy=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9 all_proxy=http://127.0.0.1:9 no_proxy= NO_PROXY="
run --url "$org_url" --port "$port"
if [ "$code" -eq 0 ] && tail -n 1 "$out" | grep -q "$link_re"; then pass; else fail "exit $code, $(cat "$err")"; fi
run --uninstall --port "$port" >/dev/null

OS=Darwin
case="15 a Mac with no login session"
fresh
EXTRA=SHIM_NOGUI=1
run --url "$org_url"
if [ "$code" -ne 0 ] && grep -q -- "--no-service" "$err" && [ ! -e "$H/.local" ] && ! grep -q bootstrap "$SHIM_LOG"; then pass; else fail "exit $code, $(cat "$err")"; fi
OS=Linux

case="16 --no-service prints a start command that survives spaces"
fresh
run --url "$org_url" --no-service --data "$H/my org"
cmd=$(sed -n 's/^Start the org server with: //p' "$out")
if [ "$code" -eq 0 ] && [ "$cmd" = "'$H/.local/bin/fonsviva' org serve --data '$H/my org' --listen 127.0.0.1:7350" ]; then pass; else fail "exit $code, command: $cmd, $(cat "$err")"; fi

case="17 uninstall with nothing at the default prefix says so"
fresh
run --uninstall --no-service
if [ "$code" -eq 0 ] && grep -q "run --uninstall with the same ones" "$out" && ! grep -q "Removed" "$out"; then pass; else fail "exit $code, $(cat "$out" "$err")"; fi

case="18 the service manager's own XDG_CONFIG_HOME"
fresh
port=$(free_port)
EXTRA="SHIM_MGR_XDG=$H/.mgr"
run --url "$org_url" --port "$port"
if [ "$code" -eq 0 ] && [ -f "$H/.mgr/systemd/user/fonsviva-org.service" ] && [ ! -e "$H/.config/systemd" ]; then pass; else fail "exit $code, $(cat "$err")"; fi
case="18 uninstall finds the unit in the service manager's XDG_CONFIG_HOME"
EXTRA="SHIM_MGR_XDG=$H/.mgr"
run --uninstall --port "$port"
if [ "$code" -eq 0 ] && [ ! -e "$H/.mgr/systemd/user/fonsviva-org.service" ] && grep -qx "systemctl --user disable --now fonsviva-org.service" "$SHIM_LOG"; then pass; else fail "exit $code, $(cat "$SHIM_LOG" "$err")"; fi

case="19 a relative XDG_CONFIG_HOME in the service manager is ignored"
fresh
port=$(free_port)
EXTRA="SHIM_MGR_XDG=cfg"
run --url "$org_url" --port "$port"
if [ "$code" -eq 0 ] && [ -f "$H/.config/systemd/user/fonsviva-org.service" ] && [ -z "$(find "$S" -path '*cfg/systemd*' 2>/dev/null)" ]; then pass; else fail "exit $code, $(cat "$err")"; fi
EXTRA="SHIM_MGR_XDG=cfg"
run --uninstall --port "$port" >/dev/null

case="20 a service manager XDG_CONFIG_HOME the unit can't carry stops the install"
fresh
EXTRA="SHIM_MGR_RAW=XDG_CONFIG_HOME=\$'$H/my\\x20cfg'"
run --url "$org_url"
if [ "$code" -ne 0 ] && grep -q -- "--no-service" "$err" && [ ! -e "$H/.local" ] && [ ! -e "$H/.config/systemd" ]; then pass; else fail "exit $code, $(cat "$err")"; fi

case="21 uninstall removes a unit left in ~/.config after the manager moved"
fresh
port=$(free_port)
run --url "$org_url" --port "$port"
EXTRA="SHIM_MGR_XDG=$H/.mgr"
run --uninstall --port "$port"
if [ "$code" -eq 0 ] && [ ! -e "$H/.config/systemd/user/fonsviva-org.service" ] && grep -qx "systemctl --user disable --now fonsviva-org.service" "$SHIM_LOG"; then pass; else fail "exit $code, $(cat "$SHIM_LOG" "$err")"; fi

case="4 a first install whose binary fails --version leaves no folders"
fresh
REL="file://$S/rel-broken"
run --url "$org_url" --version "$tag"
if [ "$code" -ne 0 ] && [ ! -e "$H/.local" ] && grep -q -- "--version failed" "$err"; then pass; else fail "exit $code, $(find "$H" | head -5), $(cat "$err")"; fi

case="4 a failed first install keeps what another program wrote meanwhile"
fresh
REL="file://$S/rel-broken-writer"
run --url "$org_url" --version "$tag"
if [ "$code" -ne 0 ] && [ -f "$H/.local/share/other/keep" ] && [ ! -e "$H/.local/bin" ]; then pass; else fail "exit $code, $(find "$H" | head -5), $(cat "$err")"; fi

case="11 no token outside org.db and the printed link"
tokens=$(cat "$S/work/out"* | sed -n 's|^https://fonsviva\.example\.org/join/\([0-9a-f]\{64\}\)$|\1|p')
[ -n "$tokens" ] || fail "no tokens were printed"
for t in $tokens ${session_tokens:-}; do
  hits=$(find "$S/homes" "$S/shimstate" "$S/work/stub.log" -type f ! -name org.db ! -name org.db-wal ! -name org.db-shm -exec grep -qF "$t" {} \; -print)
  [ -z "$hits" ] || fail "a token is in $hits"
  for f in "$S/work/out"* "$S/work/err"*; do
    if grep -F "$t" "$f" | grep -vqx "https://fonsviva\.example\.org/join/$t"; then fail "a token is in $f outside its link line"; fi
  done
done
for f in .profile .bashrc .bash_profile .zshrc .zprofile .config/fish/config.fish; do
  for h in "$S"/homes/*; do
    [ ! -e "$h/$f" ] || fail "$h/$f was written"
  done
done
[ "$failures" -gt 0 ] || pass

if [ "$failures" -gt 0 ]; then
  echo "$failures failed"
  exit 1
fi
echo "all install cases passed"
