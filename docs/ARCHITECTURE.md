# Architecture: the node daemon and the org server

Fonsviva is one Rust binary that runs as two processes. Both run on the same machine for one person; an org runs one org server and a node daemon on each machine it shares.

## The node daemon

`fonsviva serve`, on 127.0.0.1:7341, one per machine. It is the node agent: nothing else runs on a machine to join it to an org. Its API is [API.md](API.md).

- **Hardware and catalog.** Detects the OS, CPU, RAM and GPUs, and offers catalog models that fit, with measured speed.
- **One model store per machine.** Lists models in Ollama, LM Studio, llama.cpp and the Hugging Face cache and serves them in place, never copied. See [MODELS](MODELS.md) and [STORAGE](STORAGE.md).
- **Engines behind one trait.** llama.cpp (downloaded and managed by Fonsviva) and Ollama (adopted when already installed); vLLM and SGLang on large GPUs where a measurement shows they win.
- **The node channel.** A paired, encrypted link between machines, used for splits planned from measured topology results. See [DISTRIBUTED](DISTRIBUTED.md).
- **RunPod.** Pods start only behind the cloud gate and the daemon's own confirmation, with spend protection always on. See [COSTS](COSTS.md).

## The org server

`fonsviva org serve`, one per org. It serves the web app and the org API at the org's URL (for the demo, fonsviva.dbaney.com, with the admin panel at `/admin`).

- **Models.** It reaches models through the node channel or any OpenAI-compatible URL.
- **Rules.** It enforces roles, limits and privacy. Every route and every tool call checks the person's role in server code, never in a prompt.
- **One store.** It keeps one SQLite store for members, sessions, invites, the audit log and, as later slices add them, threads, memory and costs.
- **The org brain.** Its agent loop runs on the server, and its tools are server code: search, artifacts, memory and compute requests. The model sees only what the tools return for that person, so it can't reach data the person isn't cleared for.

The org's machines reach the org server over a Cloudflare tunnel, the org's own VPN or whatever network it already has. The org server listens on loopback and the tunnel or reverse proxy fronts it.

## Sign-in

In this order: one-time links first, then OpenID Connect (Microsoft, Google, Okta), then SAML. Never a paid identity service.

Disabling a member ends their sessions, and disabling or demoting an admin revokes the live links they made. An admin can step down only while another admin is signed in. A link shared with a class still works for anyone who holds it, a disabled person included, so to keep someone out, revoke that link and share a new one.

## Models and chat

An admin adds model sources: any OpenAI-compatible URL, with an optional API key. A source counts as the org's own when its address is on loopback, a private or carrier-grade NAT range, link-local or an IPv6 ULA, or its host is `localhost`; a host name is resolved when the source is added (5 s at most) and counts only when every address it gets is one of those. A local source's name is checked again on every connection, and when it no longer resolves only to those addresses the source fails with `left the org's network` and nothing is sent. Any other host, a name that doesn't resolve included, is a cloud source: it must use https, and adding one shows a notice naming the host messages are sent to. Adding a keyed source while sealed keys are stored and `secrets.key` is missing is refused, so a new key never hides the old ones. The key is sealed with ChaCha20-Poly1305 under `secrets.key`, a 32-byte key next to `org.db` and never inside it, and is sent only to that source. Without the right `secrets.key` the source fails and nothing is sent to it.

Members see each source's models as `<source name>/<model>`, asked from the source at most every 30 seconds; a source that fails is left out and shows its last error to admins. The chat proxy rewrites only `model` (and asks a stream to include usage), streams the answer back as it arrives with its status and content type, and drops the upstream request when the client goes away. Students see models and chat only when an admin turns that on. Each request sent toward a known model writes one usage row with the member, model, status, token counts and time, and never any message text; when the source fails before its models are listed, the row names only the source. A source's own 401 or 403 reaches the member as 502 `upstream_auth`, so it never reads as the member's session ending. Adding or removing a source and changing a setting are written to the audit log.

For harnesses, each member makes personal keys (`fvk_...`, up to 20, kept only as SHA-256) and points the harness at the org's `/v1` with one of them. A key acts as its member: the same role, the same daily token limit an admin may set for users and students, and the same usage rows, which members see for themselves and admins see for everyone. Disabling a member deletes their keys. A member held to the limit may have at most 4 chats in flight, so requests sent at once can't all pass on the same total; a reached limit is 429 with `Retry-After` set to the next UTC midnight. Errors on `/v1` carry OpenAI's category in `type` and Fonsviva's code in `code`.

## Clients

- **Web app.** TypeScript, Preact and Vite, embedded in the binary and served by the org server.
- **Desktop app.** The Tauri shell around the same web app.
- **Mobile apps** come later.

## What it builds on

| Layer | Project | What it gives us |
|---|---|---|
| Recipes | [Local AI registry](https://github.com/sybil-solutions/local-ai-registry) | One tested launch recipe per model and GPU, each with evidence that it passed six gates: load, chat, reasoning, tools, context, and 15 tok/s or more. |
| Engines | llama.cpp, Ollama, vLLM, SGLang | Inference. We don't use TGI (archived) and don't treat exo as a building block. |
| Harness setup | [harness-bridge](https://github.com/0xSero/harness-bridge), [omarchy-local-ai](https://github.com/0xSero/omarchy-local-ai) | Pointing coding harnesses at a local endpoint without editing their configs by hand. |

## Harnesses: Hermes and the rest

Fonsviva gives inference to the harnesses people already use: Hermes, omp, T3 Code (through the Local Studio plugin) and anything OpenAI-compatible. Linking one takes one step: the person picks the harness, and Fonsviva writes or shows its endpoint and a key of its own, held to the limits the admin set for that person. Hermes is a first-class target, not a cloud provider. The person never edits a config file by hand.

## Cloud and privacy

Cloud providers (API keys, RunPod, Colab) are off until an admin adds one. Each add shows a plain notice saying what leaves the org's machines and who can see it. The org's privacy settings decide whether messages and API requests are stored and whether admins can see them, and each person sees who can see what: the notice at first login and the lock icon by the message box.

## Extensions

Extensions come from any repo, and agents install them. A "Fonsviva ready" repo has the GitHub topic `fonsviva-ready` and a root `fonsviva.json` saying what it is and what it needs. There is no store, and installing needs no approval. [Gantrel](https://github.com/Students-of-Local/Gantrel) is an example.

The extension interface is open enough that an extension can add these:

- MCP tools in both directions.
- Inbound webhooks with their own URL and secret.
- Outbound calls to an agent the person names.

All of it runs as the person who installed it, under their role and limits. Anything an extension sends off the org's machines (an outbound call, a remote MCP server, a hosted agent such as Grok Bot) is treated like a cloud provider: off until an admin allows that destination, with the same notice saying what leaves and who can see it. Reach in is gated the same way: an inbound webhook or an MCP endpoint that people outside the org can call stays off until an admin allows it, with a notice saying who can reach in and as whom. Extension code gets only the capabilities its `fonsviva.json` declares, granted per person under their role, and never runs with the org server's privileges. Installing needs no approval; reach in or out does.

Example, not built in: a person links their own Hermes or Grok Bot to their Fonsviva. Their agent can then chat with Fonsviva, and Fonsviva can chat with their agent. If an extension like that can't be written, the extension interface is missing something.
