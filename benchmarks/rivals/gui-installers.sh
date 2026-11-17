#!/usr/bin/env bash
# GUI-only rivals: download each official Linux installer in a fresh container
# to measure bytes and seconds. Nothing is run. Steps come from each app's own docs.
# Usage: gui-installers.sh RUN [APP...]   APP: jan gpt4all msty pinokio
source "$(dirname "$0")/lib.sh"
RUN=$1
shift
APPS=${*:-jan gpt4all msty pinokio}

for APP in $APPS; do
  (
    init_run "$APP" gui-installer "$RUN"
    meta measured "GUI only, not measured headless (installer download timed, not run)"
    case "$APP" in
      jan)
        URL=https://app.jan.ai/download/latest/linux-amd64-appimage
        meta docs "https://www.jan.ai/docs/desktop/quickstart, https://www.jan.ai/docs/desktop/install/linux"
        act "download the AppImage from jan.ai"
        act "chmod +x ./Jan_{version}_amd64.AppImage"
        act "./Jan_{version}_amd64.AppImage"
        act "type the prompt (Jan downloads its default model on first launch)"
        note "No headless local path: since Desktop 0.8.5 the jan CLI is Jan Agent, which has no local inference engine; the old 'jan serve' commands are gone."
        note "Docs recommend Flatpak (flatpak install flathub ai.jan.Jan, flatpak run ai.jan.Jan); stock Ubuntu has no flatpak, so that path adds apt install flatpak and flatpak remote-add."
        note "AppImages need libfuse2 (libfuse2t64 on Ubuntu 24.04), which stock Ubuntu 24.04 lacks; Jan's docs do not mention it."
        note "The default model's size is not stated in the docs and was not measured."
        ;;
      gpt4all)
        URL=https://gpt4all.io/installers/gpt4all-installer-linux.run
        meta docs "https://docs.gpt4all.io/gpt4all_desktop/quickstart.html"
        act "download the Linux installer (.run)"
        act "chmod +x gpt4all-installer-linux.run (not in the docs, required to run it)"
        act "run the installer and click through its wizard (several screens, counted as one)"
        act "open GPT4All"
        act "click Start Chatting"
        act "click + Add Model"
        act "download a model (docs suggest Llama 3)"
        act "go to Chats"
        act "click Load Default Model"
        act "type the prompt"
        note "This is the online installer; it downloads the app components during the wizard, so these bytes are only the installer. The offline installer on GitHub v3.10.0 is 732,745,048 bytes."
        note "Last desktop release v3.10.0 is from 2025-02-25. Headless path measured separately: gpt4all.sh (Python SDK)."
        ;;
      msty)
        URL=https://next-assets.msty.studio/app/latest/linux/MstyStudio_x86_64.AppImage
        meta docs "https://docs.msty.ai/studio/getting-started/download, https://docs.msty.ai/studio/getting-started/quick-start"
        act "download Msty Studio Desktop (x64 AppImage)"
        act "chmod a+x ./MstyStudio_x86_64.AppImage"
        act "./MstyStudio_x86_64.AppImage --no-sandbox"
        act "choose Just exploring, For work or I'm a developer"
        act "accept the recommended local model (Msty installs Ollama or llama.cpp plus the model)"
        act "choose Open a normal conversation"
        act "type the prompt"
        note "Docs: 'You may need to install appropriate packages like libfuse2' to run the AppImage."
        note "Msty Sidecar is a separate tray app that serves a desktop to the browser app, not a headless local model path."
        ;;
      pinokio)
        URL=$(curl -fsSL https://api.github.com/repos/pinokiocomputer/pinokio/releases/latest |
          python3 -I -c 'import json,sys; print([a["browser_download_url"] for a in json.load(sys.stdin)["assets"] if a["name"].endswith(".AppImage") and "arm64" not in a["name"]][0])')
        meta docs "https://github.com/pinokiocomputer/pinokio (releases), https://pinokio.co"
        act "download the AppImage from GitHub releases"
        act "chmod +x Pinokio-*.AppImage"
        act "run the AppImage"
        act "first-run settings: confirm the Pinokio home folder and save"
        act "open Discover and pick a chat app script (for example Open WebUI or Ollama)"
        act "click Install and confirm"
        act "click Start"
        act "download a model inside that app"
        act "type the prompt"
        note "Pinokio is a launcher for other apps' install scripts, not a model runner; the steps after Install depend on the chosen app. Counted as a lower bound."
        note "Pinokio installs its own conda, git and node on first use, which adds downloads not measured here."
        ;;
    esac
    meta model "n/a"
    meta installer_url "$URL"
    start_container
    phase prereq "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq curl ca-certificates"
    phase install "curl -fL -o /dev/null -w '%{url_effective} %{size_download} bytes in %{time_total}s\n' '$URL'"
    finish
  )
done
