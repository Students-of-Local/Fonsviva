# Fonsviva

Fonsviva is an open source platform for organizations (universities, companies, libraries, community hubs) to share their own compute. An admin plugs in compute, and in about three clicks members are in and chatting with models that run on the org's machines.

The name is Latin for "living fountain," from Jeremiah 2:13: "they have forsaken me, the fountain of living waters, and hewed them out cisterns, broken cisterns, that can hold no water."

## What it will do (planned)

Nothing below is built yet. See [SUPPORTED.md](SUPPORTED.md) for what works today.

- **Org brain.** A built-in assistant, always there to chat, tune model speed and help admins run the org.
- **One thread per person.** Each person has one continuous, fast thread, and all of it is searchable. Memory is kept for the org and for each person.
- **Amorphous chat.** Answers show views inline, and big artifacts open in a side panel.
- **Fonsviva Search.** Private, self-hosted crawling and a headless browser, with no outside search service.
- **Computer use** and basic agent tools built in; people add MCP servers and connectors.
- **Compute requests.** A person asks for a set time, an admin approves, and the job runs. Prompts can be scheduled overnight.
- **Costs in one place.** Power, API keys, RunPod and Colab, all shown together.
- **Roles.** Admin, full use (guaranteed access to a model at any time), user and student. Admins make admins.
- **School sign-in.** One-time links, then Microsoft, Google and Okta through OpenID Connect, then SAML.
- **Privacy the org sets.** The org decides whether messages and API requests are stored and whether admins can see them, and every person is told who can see what.
- **"Fonsviva ready" extensions.** Agents install extensions from any repo. There is no store, and installing needs no approval; reaching outside the org, in or out, needs an admin's allow.

On every machine it joins, Fonsviva also:

- **Manages the models already there.** One list from Ollama, LM Studio, llama.cpp and the Hugging Face cache, served in place, never copied. See [docs/MODELS.md](docs/MODELS.md) and [docs/STORAGE.md](docs/STORAGE.md).
- **Picks models that fit.** It detects the hardware and offers catalog models that fit it, with speed measured, not guessed.
- **Runs a model across machines** over a cable or the network, through its paired, encrypted node channel, when a measured split wins. See [docs/DISTRIBUTED.md](docs/DISTRIBUTED.md).
- **Rents RunPod GPUs safely**, with the cost shown before and while they run, and spend protection that is always on. See [docs/COSTS.md](docs/COSTS.md).

It still runs for one person on one machine: the same binary manages local models, serves them through an OpenAI-compatible API and wires them into coding agents. There are no paid subscriptions.

Agents setting Fonsviva up for someone: start at [docs/agents.md](docs/agents.md) or [llms.txt](llms.txt). Site: https://students-of-local.github.io/Fonsviva/

## Built on

Fonsviva builds on open work instead of reinventing it:

- [Local AI registry](https://github.com/sybil-solutions/local-ai-registry) ([local.sybilsolutions.ai](https://local.sybilsolutions.ai)): tested model recipes per GPU and the six gates every recipe must pass.
- Engines: llama.cpp for Macs, CPUs and small cards, Ollama where it is already installed, and vLLM or SGLang for large GPUs.
- [harness-bridge](https://github.com/0xSero/harness-bridge) and [omarchy-local-ai](https://github.com/0xSero/omarchy-local-ai): pointing coding harnesses at a local endpoint.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the node daemon, the org server and what each adds.

## Principles

- **Local first.** Models run on the org's own hardware, and its data stays with it unless an admin adds a cloud provider or allows an extension to reach outside.
- **Easy for anyone.** Someone who has never heard of quantization can still pick the right model.
- **Honest.** Speed, fit and quality claims come from real measurements, never guesses.
- **Stable.** Other apps depend on Fonsviva, so it ships on Stable and Dev channels only.

## Status

Early planning. Contributions and ideas are welcome.

## License

MIT
