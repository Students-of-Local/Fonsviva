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

For harnesses, each member makes personal keys (`fvk_...`, up to 20, kept only as SHA-256) and points the harness at the org's `/v1` with one of them. A key acts as its member: the same role, the same daily token limit an admin may set for users and students, and the same usage rows, which members see for themselves and admins see for everyone. Disabling a member deletes their keys. When the source answered 2xx (a stream it cut included) and left a count out, the count is filled with an estimate and the row is marked `estimated` in usage. The prompt estimate is four bytes a token of the `tools`, `functions`, `tool_choice` and `response_format` sent and of the text, tool arguments and content parts in `messages`, plus 85 tokens for each `image_url` part and, for each `input_audio` or `file` part, 85 or a token per 16 bytes of it, whichever is more, whatever the size of the body; the completion estimate is four bytes a token of the text received (content, reasoning and tool calls). An error answer from the source is never estimated. A member held to the limit may have at most 4 chats in flight, and each holds its charge until its row is written: its prompt estimate, and the larger of `max_completion_tokens` and `max_tokens` (else 1024) times its `n`, never more than what is left. Held charges bound what is let in at once, not what one request spends: `max_tokens` goes upstream as asked, and a request is let in only while held charges leave room, so a member can end the day past the limit by about one request's completion. Today's tokens at the limit are 429 `daily_limit` with `Retry-After` set to the next UTC midnight; a fifth request in flight, or one while held charges fill the rest of the limit, is 429 `too_many_in_flight` with `Retry-After: 1`. Errors on `/v1` carry OpenAI's category in `type` and Fonsviva's code in `code`.

## Limits

An admin sets each member's limits with `PUT /members/{id}/limits` `{models, dailyTokens}`, and a member reads their own with `GET /me/limits`, which also says whether they may chat right now. Each field is null when unset, and all nulls remove the row. `models` lists the `<source>/<model>` ids a member may use (up to 200; an empty list allows none): only those are listed in `/models` and `/v1/models`, and a chat or thread send to any other is 403 `model_not_allowed`. `dailyTokens` (1 to 1,000,000,000) holds that member whatever their role, full included, and replaces the org's default, which still holds users and students who have none of their own. Checks run in order: role and the student switch, models, then the daily limit and in-flight, and a refusal writes no usage row and stores no message. Admins are never limited, though limits stored for them are kept. A personal key holds its member to the same limits. Each change writes `limits.update` naming the member and the fields that changed, never their values.

## Threads

Each member has one continuous thread with the org's models, kept on the org server. Messages live in one `messages` table and its FTS5 index, kept in step by triggers, and nowhere else: never in usage rows, the audit log, logs or errors. A message holds at most 256 KiB of text and never only whitespace. Members read their own thread newest first a page at a time, search it (1 to 200 characters, not only whitespace; each word is matched as plain text, so FTS syntax in a search does nothing, and the member filter is in the query itself), delete one message or delete the whole thread. A search snippet sets each match between U+E000 and U+E001, two private use characters the store strips from every message it keeps, so a client can tell a match from the member's own brackets. Neither delete is audited, since it is the member's own data. The store overwrites deleted rows (`secure_delete`), and after either delete it merges the search index in bounded steps until the deleted words are gone, then empties the write-ahead log, so the text is left nowhere in `org.db` or `org.db-wal`. When the bounded merges leave words behind, or another reader keeps the log from emptying after five more tries 50 ms apart, the delete still stands and the answer is 503 `busy` with `Retry-After: 1`; sending it again finishes the cleanup. A retried message delete answers 404 once it is done, since the message is already gone, and 503 again while merges remain or the log stays held; a retried thread delete answers 204. The pending cleanup is kept in the store, also when the cleanup fails with an error, so it outlives a restart, and the server runs it once when it starts; another member's delete of nothing is always 404. Two settings decide privacy, and every member can read both at `GET /privacy` for the lock notice: `keepThreads` (on by default; off shows an empty thread and stores nothing new, and deletes nothing already kept) and `adminsReadThreads` (off by default; on lets an admin read a member's thread, and each read that shows a kept thread writes a `thread.read` audit row naming the member). A disabled member's thread stays, and only an admin reads it, under that setting.

A member sends a message with `POST /thread/messages` `{text, model}`. It goes through the same chat proxy as the chat routes, with the same model, student, daily limit and in-flight checks and the same usage row, and upstream the request is only `{model, messages, stream: true}` with `stream_options.include_usage` set, as on every proxied stream: the member's earlier messages, oldest first, the newest 100 within 200 KiB of text found in the last 1,000, then the new one, with U+E000 and U+E001 stripped as the store strips them. A message with the same role as the newer one kept after it (the new user message first) is left out before the limits are counted, so roles alternate after a failed send or a single delete; the stored rows stay as they are. A send body is at most 4 MiB, room for the text and history limits with every character escaped. With `keepThreads` off the client sends that context as `history: [{role, text}]` under the same limits, and nothing is stored. The user message is stored once every check has passed, just before the chat goes upstream, so a refused send stores nothing. The answer is a stream of events: `start` with the user message's seq, a `delta` for each piece of the reply, and `done` with the reply's seq, its status and, for a source that failed or refused its key, an `error` code (`upstream`, `upstream_auth`). A send that fails before its message is stored is a plain error, as on the chat routes. Each write reads `keepThreads` again, so turning it off during a send stores nothing more. The reply is stored once it ends, with its status: what arrived when the client left (499) or the source cut it (502), nothing when the source answered an error, and at most 256 KiB. A reply whose user message was deleted while it ran is not stored. A member has one send at a time; another while it runs is 409 `busy`.

## Costs

An admin prices each source with `PATCH /sources/{id}` `{pricing}`: USD per million prompt and completion tokens as decimal strings of at most 6 places (0 to 10000), the machine's draw in `watts` for a source the org runs itself, and per-model overrides that replace either token price. The org's `electricityPerKwh` setting (default `"0.15"`) prices that draw. Prices are kept as integer micro-USD and costs as integer nano-USD, never a float; every `costNanos` goes out as a decimal string of nano-USD, so a JavaScript client never rounds it. Each usage row's cost is worked out once, when the row is written, from the pricing at that moment, so a later price change never rewrites an old row: prompt and completion tokens at their prices plus, when the source answered (2xx, or 499 when the member left), `watts` times the request's milliseconds at the electricity price, rounded down. A row is marked unknown, and its unpriced tokens cost 0, when its source has no pricing, or when it used prompt or completion tokens that no price covers and the source has no `watts`; a row whose tokens were estimated has an estimated cost. When stored pricing or the electricity price can't be read, the row is still written, at cost 0 and unknown, so the daily limit still counts it; the source lists with no pricing, its models still answer, and a `PATCH` replaces or clears it. Sums saturate at the largest integer instead of failing. Admins read each member's cost in `/usage` and totals per member or model in `/usage/totals`, whose `costKnown` is true only when every row in it was priced; members read their own in `/me/usage`. A pricing change writes a `source.update` audit row naming the fields that changed, never a price or a model id.

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
