"""Print the compressed download size of a linux/amd64 container image.

Usage: python3 -I image_bytes.py REGISTRY REPO TAG
Example: python3 -I image_bytes.py ghcr.io open-webui/open-webui ollama

Sums the config and layer sizes from the registry manifest, which is what
docker pull downloads on a host that has none of the layers.
"""

import json
import sys
import urllib.request

ACCEPT = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])


def get(url, token):
    req = urllib.request.Request(url, headers={"Accept": ACCEPT, "Authorization": "Bearer " + token})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def main():
    registry, repo, tag = sys.argv[1:4]
    if registry == "ghcr.io":
        token_url = "https://ghcr.io/token?scope=repository:%s:pull" % repo
    else:
        token_url = "https://auth.docker.io/token?service=registry.docker.io&scope=repository:%s:pull" % repo
        registry = "registry-1.docker.io"
    with urllib.request.urlopen(token_url, timeout=30) as r:
        tok = json.load(r)
    token = tok.get("token") or tok.get("access_token")
    base = "https://%s/v2/%s/manifests/" % (registry, repo)
    m = get(base + tag, token)
    if "manifests" in m:
        pick = [d for d in m["manifests"]
                if d.get("platform", {}).get("os") == "linux" and d.get("platform", {}).get("architecture") == "amd64"]
        m = get(base + pick[0]["digest"], token)
    print(m["config"]["size"] + sum(layer["size"] for layer in m["layers"]))


if __name__ == "__main__":
    main()
