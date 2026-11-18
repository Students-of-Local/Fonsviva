# Running models across devices

> **Status: build step 1 done (probe and node channel); the rest is design.** Nothing here is supported until its test passes and [SUPPORTED.md](../SUPPORTED.md) lists it.

Donovan's goal (2026-11-17): run models across devices very well, from the Local AI registry's recipes, with devices joined by a cable or by the network alone, use RunPod well, and be the best at it.

"Best" here means one measurable thing: for any set of a person's machines and any model, Fonsviva picks the way to run it that is fastest **as measured on those machines**, says why, and keeps that choice from getting slower release to release. Everything below serves that.

## Done check

1. `fonsviva bench topology` measures, on real machines, decode and prompt tok/s for each way a model can run there (one machine, or split across machines over each link they share), and writes `benchmarks/results/topology/<date>-<fleet>-<model>.json` with the link, engine and model versions.
2. `fonsviva plan <model>` names the fastest measured option and the numbers behind it. When no split beats one machine, it says so and runs on one machine.
3. Ratchet tests in CI (below) fail when the planner picks an option that measured slower than another recorded one, or when a result for the same fleet, model and versions drops more than 10% without a written reason.
4. `BENCHMARKS.md` carries a "By topology" table generated from those files, and SUPPORTED.md lists each topology only after its row exists.

## What the fleet measured on 2026-11-17

The owner's own machines, measured before any design choice:

| Machine | Compute | Memory |
| --- | --- | --- |
| Mac (MacBook Pro) | Apple M5, Metal | 32 GB unified |
| parish1 | RTX 4080 Laptop, 12 GB (8 GB already held by other work) | 31 GB, about 3 GB free |
| abbey1 (WSL) | GTX 1650 SUPER, 4 GB; 12 CPU threads | 27 GB |
| chapel1 | 8 CPU threads, no GPU | 23 GB |

Links from the Mac to parish1, 20 pings at 0.2 s:

| Path | Average | Max |
| --- | --- | --- |
| Thunderbolt cable, default route | 32.8 ms | 180 ms |
| Thunderbolt cable, socket bound to the cable address (`ping -S`) | 0.33 ms | 0.39 ms |
| Wi-Fi LAN | 16.1 ms | 93.8 ms |
| Tailscale (direct) | 14.3 ms | 72.7 ms |

Two facts follow. The cable is about 50 times faster than Wi-Fi, but only when the socket is bound to its address: both ends get link-local 169.254/16 addresses, and macOS keeps that route on Wi-Fi, so an unbound connection silently crosses Wi-Fi (field note "Thunderbolt cable link to parish1 runs at Wi-Fi latency"). Linux routes it out `thunderbolt0` on its own. And the pinned llama.cpp release Fonsviva already downloads (`b12477`) ships `ggml-rpc-server`, `libggml-rpc` and `llama-bench`, so a split across vendors needs no custom build.

## Rules the design follows

- **One machine first.** A split costs a network round trip per token. If a smaller quant fits one machine and is faster, run that. A split is offered only when its measured tok/s beats every single-machine option for the same model and quant, or when no single machine can hold the model at all (then the trade is stated: slower, but it runs).
- **Pipeline split over ordinary links, tensor split only over RDMA.** Splitting by layers sends one activation per token per boundary and tolerates a millisecond link. Splitting each layer needs microsecond links (Thunderbolt 5 RDMA between Macs with JACCL, InfiniBand or RoCE). Fonsviva never offers a tensor split over Ethernet, Wi-Fi, Tailscale or a Thunderbolt IP bridge.
- **Arithmetic before measurement.** A layer split over a link with round trip `r` caps decode at about `1 / (r × boundaries)` tokens a second before any compute. At Tailscale's 14 ms that is about 70 tok/s with one boundary at best, and far less with jitter; at the cable's 0.33 ms it is about 3,000. The planner skips options whose ceiling is already below the best single-machine result, so the bench never wastes an hour proving it.
- **Bind to the link in Fonsviva, not in the engine.** `llama-server --rpc` takes only `host:port` and cannot pick a source address, so on the Mac its connection to a link-local peer would leave by Wi-Fi. Engines therefore never open a socket between machines. Each one talks to 127.0.0.1, and Fonsviva's node channel carries the bytes between machines on a socket it binds to the chosen link (source address on Linux, `IP_BOUND_IF` on the cable's interface on macOS). The probe checks the route the kernel actually uses (`route -n get` on macOS, `ip route get` on Linux) and the bench measures through that channel, so its cost is in every number.
- **Untrusted engines stay off the network.** `ggml-rpc-server` has no authentication and its own docs call it insecure. Fonsviva starts it bound to 127.0.0.1 only. A link-local address is not an access boundary: both macOS and Linux accept a packet for a local address on any interface, so a LAN neighbor could reach a server bound to the cable address. The only way in from another machine is Fonsviva's node channel, which accepts only a peer holding the pairing key from [COMPUTE](COMPUTE.md) and encrypts the link. Inside a machine there is no gate: `llama-server --rpc` speaks only TCP, so any local process on either paired machine can reach the engines while a plan runs. Pairing therefore trusts both machines whole, the planner says so before the first split, and the channel's listeners close when the plan ends. On WSL, Windows localhost forwarding counts as local. The test connects to the node's other addresses and expects a refusal.
- **Plan for node loss.** The pinned RPC client aborts the whole `llama-server` process when a node drops, so Fonsviva runs the controller as its own child process per plan. When it exits mid-answer, the request fails with a plain reason naming the node, and the next request starts a new controller without that node. A link can also die silently (a pulled cable, a sleeping node) with no close to relay, so the node channel sends a heartbeat every second and, after 5 s without one, closes the controller's loopback socket and stops the controller. The tests kill a node mid-answer, and separately take the link's interface down mid-answer, and expect a plain failure within 10 s.
- **Registry recipes are inputs, not truth.** A Local AI registry recipe says a model passed six gates on one GPU. Fonsviva starts from it (weights pinned to a commit, engine image to a digest), then measures on the person's own machines, and shows both: the registry's evidence and the local result.

## Parts

### 1. Topology probe (`fonsviva-core::topology`)

For this machine and each paired node: compute and free memory (from `hardware.rs`), each link they share (cable, LAN, tailnet), its round trip bound to that link's address, the route the kernel uses for it, and whether RDMA exists (`ibv_devices` on Linux; `rdma_ctl status` on macOS 26.2 and later). Output is a `Topology` value saved with the result files, so a bench result always names the exact links it ran over.

### Node channel (`fonsviva-core::channel`, `fonsviva node`)

Built in step 1. Noise `NNpsk0_25519_ChaChaPoly_BLAKE2s` over TCP, every socket pinned to one interface (`IP_BOUND_IF` on macOS, `SO_BINDTODEVICE` on Linux), a heartbeat every second and a close after 5 s of silence. The key comes from pairing: `fonsviva node pair --link <interface>` on the node shows a 10-digit code, and `fonsviva node pair --link <interface> --peer <addr:port> --code <code>` on the other machine runs SPAKE2 with it, so the key never crosses the wire and is saved on each side readable only by its user. The node takes one real attempt per code; a connection that does not open with a SPAKE2 message is dropped without ending the wait, because local services probe new listeners (local-studio's core on parish1 does, within a second). `fonsviva node serve` carries tunnels from a paired machine to an engine on 127.0.0.1, `fonsviva node forward` gives the controller a loopback end, and `fonsviva node echo` with `fonsviva node rtt` measure the channel.

The node's firewall must let the pairing port (7350 by default) and the channel port in from the peer's link address on that interface. parish1 drops inbound by default, so a node there needs a rule such as `ufw allow in on thunderbolt0 from 169.254.87.94 to 169.254.222.99 port <port> proto tcp`.

Measured on 2026-11-18, Mac to parish1 over the cable through the channel, 2,000 round trips of 64 bytes: 0.257 ms average, 0.380 ms p99 (`benchmarks/results/topology/2026-11-18-mac-to-parish1-channel.json`). The bytes crossed bridge0, a wrong key failed the first handshake message, and parish1's engine port refused every address but loopback. Bulk transfer is the cost: 50 MB echoed at 156 MB/s each way through the channel against 1,226 MB/s for raw TCP on the same cable. That touches model loading over RPC more than decode, and `ggml-rpc-server --cache` keeps weights on the node after the first load; step 3 measures it.

### 2. Options and the planner (`fonsviva-core::plan`)

For a model and a topology, list every option: each machine alone at each quant that fits (`catalog::fit_best`), and each layer split across 2 or more machines over one link, with layers in proportion to free memory. Drop options whose link ceiling is below the best measured single-machine decode. Order the rest by measured tok/s from `benchmarks/results/topology/`; an option never measured is shown as "not measured" and never ranked above a measured one.

### 3. Engines

- **llama.cpp RPC** (first): each node runs `ggml-rpc-server` from the same pinned release on 127.0.0.1; the controller runs `llama-server --rpc 127.0.0.1:<port>`, a local end of the node channel. Works across Metal, CUDA, Vulkan and CPU, and either machine can be the controller.
- **MLX distributed** (second, Macs only): the ring backend over Thunderbolt IP; JACCL when both Macs have Thunderbolt 5 RDMA enabled. Added only when a Mac to Mac measurement beats llama.cpp RPC on the same machines.
- **vLLM pipeline parallel over Ray** (later, NVIDIA Linux nodes): only when two such nodes exist and a measurement wins.

exo, distributed-llama and GPUStack stay references: exo has no Linux GPU path, distributed-llama only takes q40 on 2^n nodes, and GPUStack has no Mac or llama.cpp worker.

### 4. RunPod, reusing what exists

RunPod is a place to run a **whole** model, never a shard of a split with home machines: a home to pod round trip of 20 to 80 ms caps a layer split far below what the pod alone does. Multi-GPU in the cloud means one pod with several GPUs, or a RunPod Instant Cluster where the GPUs share a fast fabric.

Fonsviva's `fonsviva-runpod` (#9, #13) already owns the key, the estimate, the native confirmation, budget caps, idle stop and orphan cleanup on the daemon side. One gap remains: when the person's computer sleeps, nothing on the daemon side can stop the pod. Hildegard's RunPod path (`deploy/runpod/boot.sh` in hildegard) already closes it, and Fonsviva reuses that pattern rather than writing a new one:

- The pod starts from the pinned template with a boot script in its environment that installs only Fonsviva's signed release, checked by SHA-256.
- The pod watches its own engine and, after the idle minutes with no request, terminates itself with the pod's own RunPod key (set by RunPod in the pod, never the person's key). It terminates rather than stops, because a stopped pod still bills for its disk; weights come back from the pinned commit on the next start.
- The person's computer joins the pod through RunPod's HTTPS proxy with a key it made and put in the pod's environment, so nothing is sent back and no UDP is needed (RunPod documents TCP and HTTP ports only, which would rule out a direct Tailscale link; not yet confirmed on a pod).

No new spend comes from this work. A real pod start still needs the person's yes for that exact charge, and the bench uses the mock RunPod server in CI.

## Ratchet tests

`crates/fonsviva-core/tests/topology_ratchet.rs`, run by CI on every pull request, reads every file in `benchmarks/results/topology/` whose schema is `fonsviva.topology-result.v1` (built in step 2 with `fonsviva-core::plan`; probe and channel files carry their own schemas and are skipped). A missing folder or any other `fonsviva.topology` schema is an error, so a typo cannot empty the ratchet. The same rules run over hand-written fixtures in `crates/fonsviva-core/tests/fixtures/ratchet/`, one folder per broken rule, and each must fail with its reason:

1. **Schema.** Each result names the fleet, every machine's hardware, each link with its measured round trip and route, the engine and its version, the model with its commit and quant, prompt and decode tok/s, and a log hash.
2. **The planner picks the measured winner.** For each fleet and model with results, each placement counts by its newest result, and `plan()` must choose the placement whose newest decode tok/s is highest. A planner change that picks a slower option fails, and so does a current result recorded as the plan (`planned: true`) that another placement's newest result beat. `measured_at` must be UTC in the form `2026-11-20T10:00:00Z`, and two results for one placement may not share it.
3. **No silent regression.** For the same fleet, model, quant and engine version, the newest decode tok/s must be at least 90% of the best earlier one, unless the result carries a `regression_note`.
4. **Splits must earn their place.** A split recorded as the plan for a model that fits one machine must beat that machine's best single result.

## Build order

Each step is its own pull request with its own check.

1. Topology probe, the node channel (paired, encrypted, bound to a chosen link) and their result schema, with a recorded probe of this fleet. Check: the probe reports the Mac's cable route as Wi-Fi unless bound, the round trip through the channel over the cable is under 1 ms, and a connection to a node's engine port from any address but loopback is refused.
2. Planner with the link ceiling rule, and the ratchet test fed by hand-written fixtures. Check: the ratchet fails on a fixture where a slower option is picked.
3. llama.cpp RPC engine, bound sockets, and `fonsviva bench topology`. Check: a real result file for the Mac alone, parish1 alone (when its GPU is free) and the Mac plus parish1 over the cable, for one model that fits both and one that fits neither alone.
4. Node loss: drain and replan. Check: killing the Mac's `ggml-rpc-server` mid-answer, and separately taking the cable's interface down, each give a plain failure within 10 s, and the next request runs on parish1 alone.
5. RunPod pod-side idle stop and proxy join, from Hildegard's boot script. Check against the mock: the pod terminates itself with the daemon gone.
6. MLX ring and JACCL, when a second Mac is on hand.

## Open questions

- parish1's GPU is shared with other work; a split that uses it needs `parish-media status` to show it free, or uses its CPU only.
- Donovan may want the cable given a fixed subnet (for example 10.77.0.1/30 and 10.77.0.2/30) so every tool uses it without binding. That is his call; Fonsviva works either way.
