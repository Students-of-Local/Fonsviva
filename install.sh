#!/bin/sh
# Installs the Fonsviva org server for the person running it: the binary in
# <prefix>/bin, the store in <data>, and a user service. Never as root.
set -eu

REPO=https://github.com/Students-of-Local/Fonsviva
LABEL=com.studentsoflocal.fonsviva-org
UNIT=fonsviva-org.service

main() {
exec </dev/null

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: install.sh --url <org url> [--version <tag>] [--prefix <dir>] [--data <dir>]
                  [--port <n>] [--no-service] [--uninstall] [--yes]
EOF
}

[ -n "${HOME:-}" ] || die "HOME is not set."
[ "$(id -u)" -ne 0 ] || die "run this as yourself, not as root."

url="" version="latest" prefix="$HOME/.local" data="$HOME/.local/share/fonsviva/org"
port=7350 service=1 uninstall=0 yes=0
while [ $# -gt 0 ]; do
  case $1 in
    --url | --version | --prefix | --data | --port)
      [ $# -ge 2 ] || die "$1 needs a value."
      case $1 in
        --url) url=$2 ;;
        --version) version=$2 ;;
        --prefix) prefix=$2 ;;
        --data) data=$2 ;;
        --port) port=$2 ;;
      esac
      shift 2
      ;;
    --no-service) service=0; shift ;;
    --uninstall) uninstall=1; shift ;;
    --yes) yes=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option $1" ;;
  esac
done

case $port in
  '' | *[!0-9]*) die "--port must be a number from 1 to 65535." ;;
esac
[ "$port" -ge 1 ] && [ "$port" -le 65535 ] || die "--port must be a number from 1 to 65535."
case $version in
  latest | v[0-9]*) ;;
  *) die "--version takes a tag such as v0.1.0." ;;
esac
case $version in
  *[!A-Za-z0-9.+-]*) die "--version takes a tag such as v0.1.0." ;;
esac
for p in "$prefix" "$data"; do
  case $p in
    /*) ;;
    *) die "$p must be an absolute path." ;;
  esac
done
url=${url%/}

os=$(uname -s)
arch=$(uname -m)
case $os in
  Linux) os_target=unknown-linux-gnu ;;
  Darwin) os_target=apple-darwin ;;
  *) die "Fonsviva has no build for $os $arch." ;;
esac
case $arch in
  x86_64 | amd64) target="x86_64-$os_target" ;;
  aarch64 | arm64) target="aarch64-$os_target" ;;
  *) die "Fonsviva has no build for $os $arch." ;;
esac

bin="$prefix/bin/fonsviva"
uid=$(id -u)
if [ "$os" = Linux ]; then
  systemctl=${FONSVIVA_SYSTEMCTL:-systemctl}
  loginctl=${FONSVIVA_LOGINCTL:-loginctl}
  unit_dir="$HOME/.config/systemd/user"
  service_file="$unit_dir/$UNIT"
  log="${XDG_STATE_HOME:-$HOME/.local/state}/fonsviva/org.log"
else
  launchctl=${FONSVIVA_LAUNCHCTL:-launchctl}
  service_file="$HOME/Library/LaunchAgents/$LABEL.plist"
  log="$HOME/Library/Logs/fonsviva-org.log"
fi

quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

manager_unit() {
  mgr_config=$("$systemctl" --user show-environment 2>/dev/null | sed -n 's/^XDG_CONFIG_HOME=//p') || mgr_config=
  case $mgr_config in
    /*) ;;
    *) mgr_config=$HOME/.config ;;
  esac
  service_file="$mgr_config/systemd/user/$UNIT"
}

prune() {
  d=$prefix/bin
  while rmdir "$d" 2>/dev/null; do
    [ "$d" != "$made" ] || break
    d=$(dirname "$d")
  done
}

service_loaded() {
  "$launchctl" print "gui/$uid/$LABEL" >/dev/null 2>&1
}

remove_service() {
  if [ "$os" = Linux ]; then
    if command -v "$systemctl" >/dev/null 2>&1; then
      manager_unit
    fi
    if [ -f "$service_file" ]; then
      "$systemctl" --user disable --now "$UNIT"
      rm -f "$service_file"
      "$systemctl" --user daemon-reload
    fi
  else
    if service_loaded; then
      "$launchctl" bootout "gui/$uid/$LABEL"
    fi
    rm -f "$service_file"
  fi
}

if [ "$uninstall" -eq 1 ]; then
  if [ "$service" -eq 1 ]; then
    remove_service
  fi
  if [ ! -e "$bin" ]; then
    say "Found no fonsviva at $bin, so nothing else was removed. If you installed with --prefix or --data, run --uninstall with the same ones."
    exit 0
  fi
  rm -f "$bin"
  say "Removed the Fonsviva org server. The org's data is kept at $data."
  exit 0
fi

if [ "$service" -eq 1 ]; then
  for p in "$prefix" "$data"; do
    case $p in
      "$HOME"/*) ;;
      *) die "$p is outside $HOME; the service only runs from your home folder. Pass --no-service to run it another way." ;;
    esac
  done
  for p in "$prefix" "$data" "$log" "$service_file"; do
    case $p in
      *[!A-Za-z0-9._/@+-]*) die "$p holds characters the service file can't carry; pick a plain path or pass --no-service." ;;
    esac
  done
  if [ "$os" = Linux ]; then
    command -v "$systemctl" >/dev/null 2>&1 || die "systemctl is not on PATH; pass --no-service to run the server yourself."
    manager_unit
    case $service_file in
      *[!A-Za-z0-9._/@+-]*) die "$service_file holds characters the service file can't carry; pick a plain path or pass --no-service." ;;
    esac
  else
    command -v "$launchctl" >/dev/null 2>&1 || die "launchctl is not on PATH; pass --no-service to run the server yourself."
    "$launchctl" print "gui/$uid" >/dev/null 2>&1 || die "there is no macOS login session for you here (launchctl gui/$uid), so a launch agent can't run. Log in on the Mac once, or pass --no-service to run the server yourself."
  fi
fi

for t in tar mktemp; do
  command -v "$t" >/dev/null 2>&1 || die "install.sh needs $t, and it is not on PATH."
done
if command -v curl >/dev/null 2>&1; then
  fetcher=curl
elif command -v wget >/dev/null 2>&1; then
  fetcher=wget
else
  die "install.sh needs curl or wget, and neither is on PATH."
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "install.sh needs sha256sum or shasum, and neither is on PATH."
fi

if [ ! -f "$data/org.db" ] && [ -z "$url" ]; then
  die "--url is required on first install, such as --url https://fonsviva.example.org"
fi

if [ -n "${FONSVIVA_RELEASE_BASE:-}" ]; then
  release=${FONSVIVA_RELEASE_BASE%/}
elif [ "$version" = latest ]; then
  release="$REPO/releases/latest/download"
else
  release="$REPO/releases/download/$version"
fi
case $release in
  https://* | file://*) ;;
  *) die "FONSVIVA_RELEASE_BASE must be an https:// or file:// address." ;;
esac

fetch() {
  case $1 in
    file://*) cp "${1#file://}" "$2" ;;
    *)
      if [ "$fetcher" = curl ]; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
      else
        wget -q --https-only -O "$2" "$1"
      fi
      ;;
  esac
}

tmp=$(mktemp -d)
staged="" made=""
trap 'rm -rf "$tmp"; [ -z "$staged" ] || rm -f "$staged"; [ -z "$made" ] || prune' EXIT
trap 'exit 130' INT TERM

fetch "$release/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $release/SHA256SUMS"
if [ "$version" = latest ]; then
  name=$(awk -v t="$target" '{ n = $2; sub(/^\*/, "", n) } n ~ "^fonsviva-v[^ ]*-" t "\\.tar\\.gz$" { print n; exit }' "$tmp/SHA256SUMS")
  [ -n "$name" ] || die "SHA256SUMS has no line for a $target build; nothing installed."
  tag=${name#fonsviva-}
  tag=${tag%-"$target".tar.gz}
else
  tag=$version
  name="fonsviva-$tag-$target.tar.gz"
fi
want=$(awk -v n="$name" '{ f = $2; sub(/^\*/, "", f) } f == n { print $1; exit }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "SHA256SUMS has no line for $name; nothing installed."
fetch "$release/$name" "$tmp/$name" || die "could not download $release/$name"
got=$(sha "$tmp/$name")
[ "$got" = "$want" ] || die "$name does not match SHA256SUMS; nothing installed."
say "Checked $name against SHA256SUMS. This release has no signature yet, so the check trusts the HTTPS download from GitHub."

mkdir "$tmp/x"
tar -xzf "$tmp/$name" -C "$tmp/x" fonsviva || die "$name holds no fonsviva binary; nothing installed."
made=$prefix/bin
while [ ! -e "$(dirname "$made")" ]; do made=$(dirname "$made"); done
[ ! -e "$made" ] || made=""
mkdir -p "$prefix/bin"
staged="$prefix/bin/.fonsviva.new.$$"
cp "$tmp/x/fonsviva" "$staged"
chmod 0755 "$staged"
if ! "$staged" --version >/dev/null 2>&1; then
  rm -f "$staged"
  die "the downloaded fonsviva does not run on this machine (--version failed); nothing installed."
fi

semver() {
  printf '%s\n' "$1" | awk 'match($0, /[0-9]+\.[0-9]+\.[0-9]+/) { split(substr($0, RSTART, RLENGTH), v, "."); printf "%d%06d%06d\n", v[1], v[2], v[3]; exit }'
}
older() {
  [ -n "$1" ] && [ -n "$2" ] && [ "$1" -lt "$2" ]
}
if [ -x "$bin" ] && have=$("$bin" --version 2>/dev/null); then
  if older "$(semver "$tag")" "$(semver "$have")" && [ "$yes" -eq 0 ]; then
    if ! (exec </dev/tty) 2>/dev/null; then
      rm -f "$staged"
      die "$tag is older than the installed $have; pass --yes to install it anyway."
    fi
    printf 'Replace the installed %s with the older %s? [y/N] ' "$have" "$tag" >/dev/tty
    read -r answer </dev/tty || answer=
    case $answer in
      y | Y | yes | YES) ;;
      *) rm -f "$staged"; die "kept the installed $have." ;;
    esac
  fi
fi

mv -f "$staged" "$bin"
staged="" made=""

if [ -f "$data/org.db" ]; then
  link=$("$bin" org admin-link --data "$data")
  stored=${link%/join/*}
  if [ -n "$url" ] && [ "$url" != "$stored" ]; then
    say "Warning: the store at $data is for $stored, not $url. It stays $stored." >&2
  fi
else
  link=$("$bin" org init --data "$data" --url "$url")
  stored=${link%/join/*}
fi

serve_cmd="$bin org serve --data $data --listen 127.0.0.1:$port"

if [ "$service" -eq 0 ]; then
  say "Fonsviva $tag is installed at $bin for $stored."
  say "Start the org server with: $(quote "$bin") org serve --data $(quote "$data") --listen 127.0.0.1:$port"
  say "Sign in as the first admin (the link works once, for 24 hours):"
  say "$link"
  exit 0
fi

mkdir -p "$(dirname "$log")" "$(dirname "$service_file")"
if [ "$os" = Linux ]; then
  cat >"$service_file.new" <<EOF
[Unit]
Description=Fonsviva org server
After=network.target

[Service]
ExecStart=$serve_cmd
Restart=always
StandardOutput=append:$log
StandardError=append:$log

[Install]
WantedBy=default.target
EOF
  mv -f "$service_file.new" "$service_file"
  "$systemctl" --user daemon-reload
  "$systemctl" --user enable "$UNIT"
  "$systemctl" --user restart "$UNIT"
  user=${USER:-$(id -un)}
  linger=$("$loginctl" show-user "$user" -p Linger 2>/dev/null) || linger=unknown
  if [ "$linger" != "Linger=yes" ]; then
    say "Warning: the org server stops when you log out. To keep it running, run: loginctl enable-linger $user" >&2
  fi
else
  cat >"$service_file.new" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin</string>
    <string>org</string>
    <string>serve</string>
    <string>--data</string>
    <string>$data</string>
    <string>--listen</string>
    <string>127.0.0.1:$port</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$log</string>
  <key>StandardErrorPath</key>
  <string>$log</string>
</dict>
</plist>
EOF
  if service_loaded; then
    "$launchctl" bootout "gui/$uid/$LABEL"
    i=0
    while service_loaded; do
      i=$((i + 1))
      [ "$i" -le 50 ] || { rm -f "$service_file.new"; die "launchd is still unloading the old agent after 10 seconds; run the installer again."; }
      sleep 0.2
    done
  fi
  mv -f "$service_file.new" "$service_file"
  "$launchctl" bootstrap "gui/$uid" "$service_file"
fi

health="http://127.0.0.1:$port/api/org/v1/health"
healthy() {
  if [ "$fetcher" = curl ]; then
    body=$(curl -fsS --noproxy '*' --max-time 1 "$health" 2>/dev/null) || return 1
  else
    body=$(wget -q --no-proxy -T 1 -O - "$health" 2>/dev/null) || return 1
  fi
  [ "$body" = '{"ok":true}' ]
}
deadline=$(($(date +%s) + 20))
until healthy; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    {
      say "The org server didn't answer on $health within 20 seconds. The binary and the store stay."
      say "Last lines of $log:"
      tail -n 20 "$log" 2>/dev/null || say "(no log yet)"
      say "Read the rest with: cat $log"
    } >&2
    exit 1
  fi
  sleep 1
done

say "Fonsviva $tag is running at http://127.0.0.1:$port for $stored."
say "Sign in as the first admin (the link works once, for 24 hours):"
say "$link"
}

main "$@"
