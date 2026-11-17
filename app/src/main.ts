import "./styles.css";
import type { Api, ApiError, CatalogEntry, Hardware, InstalledModel, Job, Pod, Running, Stats, WireTarget } from "./api";
import { connect } from "./daemon";
import { pullAndWait } from "./pull";

type View = "home" | "models" | "connect" | "cloud" | "settings";

const app = document.getElementById("app")!;
let api: Api;
let sample = false;
let view: View = "home";
let hw: Hardware | null = null;
let running: Running | null = null;
let lastStats: Stats | null = null;

type Child = Node | string | null | undefined | false;
function h(tag: string, attrs: Record<string, unknown> = {}, ...kids: Child[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v as EventListener);
    else if (v !== false && v != null) el.setAttribute(k, String(v));
  }
  for (const c of kids) if (c !== null && c !== undefined && c !== false) el.append(c);
  return el;
}

const gb = (b: number) => (b / 1e9 >= 10 ? Math.round(b / 1e9) : (b / 1e9).toFixed(1)) + " GB";
const num = (n: number) => n.toLocaleString("en-US");
const money = (n: number) => "$" + n.toFixed(2);
const fitChip = (fit: string, reason: string) =>
  h("span", { class: `chip ${fit === "fits" ? "fit" : fit}`, title: reason }, fit === "fits" ? "Fits well" : fit === "tight" ? "Tight" : "Too big");
const say = (e: unknown) => {
  const err = (typeof e === "string" ? { message: e } : (e ?? {})) as Partial<ApiError>;
  return h("p", { class: "note err", role: "alert" }, err.message ?? "Something went wrong.", err.fix ? " " + err.fix : "");
};
/** Runs an action from a button; a failure shows next to `where` instead of vanishing. */
const act = (where: () => Element | null | undefined, f: () => Promise<unknown>) => async () => {
  try { await f(); } catch (e) { where()?.append(say(e)); }
};

const ICON: Record<View, string> = {
  home: "M2 7.5 8 3l6 4.5V13H2z",
  models: "M2.5 4.5a2 2 0 0 1 2-2h7a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2h-7a2 2 0 0 1-2-2zM5 6h6M5 9h4",
  connect: "M6 10 10 6M5 7 3.5 8.5a2.5 2.5 0 0 0 3.5 3.5L8.5 11M11 9l1.5-1.5A2.5 2.5 0 0 0 9 4L7.5 5.5",
  cloud: "M4.5 12.5h7a3 3 0 0 0 .3-6 4 4 0 0 0-7.7-.9A3.4 3.4 0 0 0 4.5 12.5Z",
  settings: "M8 5.8a2.2 2.2 0 1 0 0 4.4 2.2 2.2 0 0 0 0-4.4M8 1.8v2M8 12.2v2M1.8 8h2M12.2 8h2M3.6 3.6 5 5M11 11l1.4 1.4M3.6 12.4 5 11M11 5l1.4-1.4",
};

function icon(d: string) {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 16 16");
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute("d", d);
  s.append(p);
  return s;
}

function shell(main: HTMLElement) {
  const nav = (v: View, label: string) =>
    h("div", { class: `nav${view === v ? " on" : ""}`, tabindex: 0, role: "button", "aria-current": view === v ? "page" : false, onclick: () => go(v), onkeydown: (e: KeyboardEvent) => e.key === "Enter" && go(v) }, icon(ICON[v]), label);
  app.replaceChildren(
    h("div", { class: "drag", "data-tauri-drag-region": "" }),
    h("div", { class: "win" },
      h("aside", { class: "side" },
        h("div", { class: "brand" }, h("b", {}, "Fonsviva")),
        nav("home", "Home"), nav("models", "Models"), nav("connect", "Connect"), nav("cloud", "Cloud"), nav("settings", "Settings"),
        h("div", { class: "foot" },
          h("span", {}, h("span", { class: "dot" }), running ? running.name : "No model loaded"),
          h("span", {}, sample ? "Sample data" : "Running on this computer"))),
      main),
    ...(sample ? [h("span", { class: "sample" }, "Sample data, no daemon")] : []),
  );
}

function go(v: View) {
  view = v;
  render();
}

async function render() {
  const main = h("main", { class: "fade" });
  shell(main);
  try {
    if (view === "home") await home(main);
    else if (view === "models") await models(main);
    else if (view === "connect") await connectView(main);
    else if (view === "cloud") await cloud(main);
    else settings(main);
  } catch (e) {
    main.append(say(e));
  }
}

async function onboarding() {
  hw = await api.hardware();
  const [cat, have] = await Promise.all([api.catalog(), api.models()]);
  const pick = cat.find((c) => c.recommended) ?? cat.find((c) => c.fit === "fits") ?? cat.find((c) => c.fit === "tight");
  if (!pick) {
    app.replaceChildren(h("div", { class: "drag", "data-tauri-drag-region": "" }),
      h("div", { class: "win solo" }, h("main", {}, h("div", { class: "center" },
        h("h1", {}, "No local model fits this computer yet"),
        h("p", { class: "sub" }, `Fonsviva found ${gb(hw.ram_free_bytes)} of free memory, which is less than the smallest model in the catalog needs. Close other apps and reopen Fonsviva, or rent a GPU on RunPod.`),
        h("div", { class: "row" }, h("button", { class: "btn primary", onclick: () => go("cloud") }, "See RunPod prices"), h("button", { class: "btn ghost", onclick: () => go("models") }, "See all models"))))));
    return;
  }
  const gpu = hw.gpus[0];
  const tile = (k: string, v: string) => h("div", {}, h("div", { class: "k" }, k), h("div", { class: "v" }, v));
  app.replaceChildren(
    h("div", { class: "drag", "data-tauri-drag-region": "" }),
    h("div", { class: "win solo" }, h("main", { class: "fade" }, h("div", { class: "center" },
      h("div", {}, h("h1", {}, "Run AI models on this computer"), h("p", { class: "sub" }, "Private, free, and ready in about two minutes.")),
      h("div", { class: "card" },
        h("div", { class: "row", style: "justify-content:space-between;margin-bottom:10px" }, h("h2", {}, "This computer"), h("span", { class: "k" }, "Found automatically")),
        h("div", { class: "hw" },
          tile("Chip", hw.cpu.name),
          tile("Memory", `${gb(hw.ram_total_bytes)}, ${gb(hw.ram_free_bytes)} free`),
          tile("Graphics", gpu ? `${gpu.backend === "metal" ? "Metal" : gpu.name}${hw.unified_memory ? ", shared memory" : ", " + gb(gpu.vram_bytes)}` : "None, CPU only"),
          tile("Disk free", gb(hw.disk_free_bytes)))),
      h("div", { class: "card hero" },
        h("div", { class: "row", style: "justify-content:space-between;align-items:flex-start" },
          h("div", {}, h("div", { class: "k" }, "Recommended first model"),
            h("h2", { style: "font:600 18px var(--serif);margin-top:3px" }, pick.name),
            h("p", { class: "sub", style: "margin-top:4px" }, `${pick.summary} ${pick.fit_reason}, with room for ${num(pick.context)} tokens of context.`)),
          fitChip(pick.fit, pick.fit_reason)),
        h("div", { class: "chips", style: "margin:12px 0 14px" },
          h("span", { class: "chip" }, `${gb(pick.download_bytes)} download`),
          pick.est_tok_s ? h("span", { class: "chip" }, `About ${Math.round(pick.est_tok_s)} tokens per second here`) : null,
          h("span", { class: "chip" }, pick.license)),
        h("div", { class: "row" },
          h("button", { class: "btn primary big", autofocus: true, onclick: () => setup(pick) }, "Get started"),
          h("button", { class: "btn ghost", onclick: () => go("models") }, "Choose a different model"))),
      have.length ? h("p", { class: "k", style: "text-align:center;margin:0" }, `Fonsviva found ${have.length} model${have.length > 1 ? "s" : ""} you already have and can use without downloading again.`) : null,
    ))),
    ...(sample ? [h("span", { class: "sample" }, "Sample data, no daemon")] : []),
  );
}

async function setup(pick: CatalogEntry) {
  const steps: Record<string, HTMLElement> = {};
  const step = (id: string, label: string) =>
    (steps[id] = h("div", { class: "step" }, h("span", { class: "ic" }), h("div", {}, label), h("span", { class: "k mono" }, "next")));
  const list = h("div", { class: "steps" },
    step("engine", "Engine (llama.cpp)"), step("pull", `Model download, ${gb(pick.download_bytes)}`),
    step("load", `Load into memory, context ${num(pick.context)} tokens`), step("bench", "Speed check on this computer"));
  app.replaceChildren(
    h("div", { class: "drag", "data-tauri-drag-region": "" }),
    h("div", { class: "win solo" }, h("main", { class: "fade" }, h("div", { class: "center" },
      h("h1", {}, `Getting ${pick.name} ready`), h("div", { class: "card" }, list),
      h("div", { class: "note" }, "The download resumes if you close the app or lose the connection. Every file is checked against its published fingerprint before it runs.")))),
  );
  const mark = (id: string, state: "now" | "done", right: string, label?: string) => {
    const s = steps[id];
    s.className = `step ${state}`;
    if (label) (s.children[1] as HTMLElement).replaceChildren(label);
    (s.children[2] as HTMLElement).textContent = right;
  };
  const t0 = performance.now();
  const show = (j: Job) => {
    if (j.kind === "engine") mark("engine", j.state === "done" ? "done" : "now", j.state === "done" ? "done" : "getting", j.label);
    if (j.kind === "pull") {
      if (j.state === "done") return mark("pull", "done", `done in ${Math.round((performance.now() - t0) / 1000)} s`);
      const s = steps.pull;
      s.className = "step now";
      const bar = h("div", { class: "bar", style: "margin-top:6px" }, h("i", { style: `width:${(100 * (j.done_bytes ?? 0)) / (j.total_bytes || 1)}%` }));
      (s.children[1] as HTMLElement).replaceChildren(h("div", {}, j.label), bar);
      (s.children[2] as HTMLElement).textContent = `${((j.done_bytes ?? 0) / 1e9).toFixed(2)} of ${gb(j.total_bytes ?? 0)}${j.seconds_left != null ? `, ${j.seconds_left} s left` : ""}`;
    }
  };
  try {
    await pullAndWait(api, pick.id, show);
    mark("pull", "done", `done in ${Math.round((performance.now() - t0) / 1000)} s`);
    mark("load", "now", "loading");
    running = await api.run(pick.id);
    mark("load", "done", `context ${num(running.context)}`);
    mark("bench", "now", "measuring");
    lastStats = await api.ask(running.model, "Name three primary colors.", () => {});
    mark("bench", "done", `${lastStats.gen_tok_s.toFixed(1)} tokens/s`);
    remember(SETUP_DONE);
    view = "home";
    render();
  } catch (e) {
    app.querySelector(".center")?.append(say(e));
  }
}

async function home(main: HTMLElement) {
  const [run] = await api.running();
  running = run ?? null;
  if (!running) {
    main.append(h("h1", {}, "No model loaded"), h("p", { class: "sub" }, "Pick one on Models, or load the one you used last."),
      h("div", {}, h("button", { class: "btn primary", onclick: () => go("models") }, "Choose a model")));
    return;
  }
  const r = running;
  const stats = (await api.stats()).find((s) => s.model === r.model) ?? lastStats;
  const stat = (k: string, v: string, unit: string) => h("div", { class: "stat" }, h("div", { class: "k" }, k), h("div", { class: "num" }, v, h("small", {}, unit)));
  const statsRow = (st: Stats | null | undefined) => h("div", { class: "stats", "aria-live": "polite" },
    stat("First word after", st ? String(Math.round(st.ttft_ms)) : "not yet", st ? "ms" : ""),
    stat("Writing speed", st ? st.gen_tok_s.toFixed(1) : "not yet", st ? "tokens/s" : ""),
    stat("Reading speed", st ? String(Math.round(st.prompt_tok_s)) : "not yet", st ? "tokens/s" : ""),
    stat("Memory in use", ((st?.memory_bytes ?? r.memory_bytes) / 1e9).toFixed(1), hw ? `of ${gb(hw.ram_total_bytes)}` : "GB"));
  let statRow = statsRow(stats);
  const chat = h("div", { class: "chat" });
  const input = h("input", { class: "input", placeholder: "Ask anything", "aria-label": "Ask the model" }) as HTMLInputElement;
  input.addEventListener("keydown", async (e) => {
    if (e.key !== "Enter" || !input.value.trim()) return;
    const q = input.value.trim();
    input.value = "";
    const out = h("div", { class: "bubble it" });
    chat.append(h("div", { class: "bubble me" }, q), out);
    try {
      lastStats = await api.ask(r.model, q, (t) => out.append(t));
      const next = statsRow(lastStats);
      statRow.replaceWith(next);
      statRow = next;
    } catch (err) {
      out.replaceWith(say(err));
    }
  });
  const wires = await api.wire();
  main.append(
    h("div", { class: "row", style: "justify-content:space-between" },
      h("div", {}, h("h1", {}, r.name), h("p", { class: "sub" }, `${r.engine} on ${r.backend}, ${r.gpu_layers} of ${r.total_layers} layers on the GPU, context ${num(r.context)} tokens`)),
      h("div", { class: "row" }, h("button", { class: "btn", onclick: () => go("models") }, "Switch model"),
        h("button", { class: "btn ghost", onclick: act(() => main, async () => { await api.stop(r.model); render(); }) }, "Unload"))),
    statRow,
    h("div", { class: "grid2" },
      h("div", { class: "card try" },
        h("div", { class: "row", style: "justify-content:space-between" }, h("h2", {}, "Try it"), h("span", { class: "k" }, "Speed is measured on each answer")),
        chat, input),
      h("div", { class: "card" },
        h("div", { class: "row", style: "justify-content:space-between;margin-bottom:10px" }, h("h2", {}, "Use it in"), h("span", { class: "k" }, "Undo any time")),
        wireList(wires))),
  );
}

const WIRE: Record<WireTarget["target"], [string, string, string]> = {
  pi: ["pi", "pi", "Adds Fonsviva models to pi; your other settings stay"],
  t3code: ["T3", "T3 Code, through pi", "Turns on T3 Code's pi provider"],
  eukhe: ["LS", "Local Studio", "Same models, one copy on disk"],
  openai: ["{ }", "Any OpenAI app", ""],
};

function wireList(wires: WireTarget[]) {
  return h("div", { class: "conn" }, ...wires.map((w) => {
    const [logo, name, note] = WIRE[w.target];
    if (w.target === "openai") {
      const url = w.base_url ?? "";
      return h("div", { class: "ci" }, h("span", { class: "logo" }, logo),
        h("div", {}, h("div", { class: "v" }, name), h("div", { class: "k mono" }, url)),
        h("button", { class: "btn", style: "padding:5px 10px", onclick: (e: Event) => { navigator.clipboard?.writeText(url); (e.target as HTMLElement).textContent = "Copied"; } }, "Copy"));
    }
    const on = w.state === "applied";
    const sw = h("button", { class: `sw${on ? " on" : ""}`, role: "switch", "aria-checked": on ? "true" : "false", "aria-label": name, disabled: w.state === "unavailable" });
    const flip = async (confirm: boolean) => {
      sw.setAttribute("disabled", "");
      sw.parentElement?.parentElement?.querySelectorAll(`[data-for="${w.target}"]`).forEach((n) => n.remove());
      try {
        on ? await api.wireRevert(w.target, confirm) : await api.wireApply(w.target, confirm);
        render();
      } catch (e) {
        sw.removeAttribute("disabled");
        const msg = say(e);
        msg.dataset.for = w.target;
        if ((e as ApiError)?.code === "drifted") {
          msg.append(" ", h("button", { class: "btn", style: "padding:3px 10px", onclick: () => flip(true) }, on ? "Undo anyway, keeping a copy" : "Apply anyway, keeping a copy"));
        }
        sw.parentElement?.after(msg);
      }
    };
    sw.addEventListener("click", () => flip(false));
    return h("div", { class: "ci" }, h("span", { class: "logo" }, logo),
      h("div", {}, h("div", { class: "v" }, name), h("div", { class: "k" }, w.state === "drifted" ? "Changed by hand since; undo keeps your edits" : w.state === "unavailable" ? (w.detail ?? "Not installed") : note)),
      sw);
  }));
}

async function models(main: HTMLElement) {
  const [have, cat] = await Promise.all([api.models(), api.catalog()]);
  const haveIds = new Set(have.map((m) => m.id));
  const FROM: Record<InstalledModel["source"], string> = { fonsviva: "Fonsviva", ollama: "Ollama", lmstudio: "LM Studio", hf: "Hugging Face" };
  const row = (name: string, quant: string, from: string, bytes: number, ctx: number, speed: string, fit: string, reason: string, action: HTMLElement) =>
    h("tr", {}, h("td", {}, running?.name === name ? h("span", { class: "dot" }) : null, h("b", {}, name), " ", h("span", { class: "k mono" }, quant)),
      h("td", { class: "k" }, from), h("td", { class: "r mono" }, gb(bytes)), h("td", { class: "r mono" }, fit === "no" ? "n/a" : num(ctx)),
      h("td", { class: "r mono" }, speed), h("td", {}, fitChip(fit, reason)), h("td", { class: "r" }, action));
  const load = (id: string) => h("button", { class: "btn ghost", style: "padding:4px 10px", onclick: act(() => main, async () => { running = await api.run(id); go("home"); }) }, running?.model === id ? "Loaded" : "Load");
  main.append(
    h("div", {}, h("h1", {}, "Models"), h("p", { class: "sub" }, "Fit is worked out for this computer at the context shown. Ollama and LM Studio models are used in place, never copied.")),
    h("div", { class: "card", style: "padding:4px 8px" }, h("table", {},
      h("thead", {}, h("tr", {}, ...["Model", "From", "Size", "Context", "Speed here", "Fit", ""].map((t, i) => h("th", { class: i >= 2 && i <= 4 ? "r" : "" }, t)))),
      h("tbody", {},
        ...have.map((m) => row(m.name, m.quant, FROM[m.source], m.bytes, m.context, m.measured_tok_s ? `${Math.round(m.measured_tok_s)} tok/s` : "not yet", m.fit, m.fit_reason, load(m.id))),
        ...cat.filter((c) => !haveIds.has(c.id) && !(c.installed_as && haveIds.has(c.installed_as))).map((c) => row(c.name, c.quant, "Catalog", c.download_bytes, c.context, c.est_tok_s ? `about ${Math.round(c.est_tok_s)} tok/s` : "n/a", c.fit, c.fit_reason,
          c.fit === "no" ? h("button", { class: "btn ghost", style: "padding:4px 10px", onclick: () => go("cloud") }, "Rent on RunPod")
            : h("button", { class: "btn ghost", style: "padding:4px 10px", onclick: () => setup(c) }, "Get")))))),
  );
}

async function connectView(main: HTMLElement) {
  main.append(h("div", {}, h("h1", {}, "Connect"), h("p", { class: "sub" }, "Each switch writes one entry into that app's settings, keeps a backup of the file, and undoes cleanly.")),
    h("div", { class: "card" }, wireList(await api.wire())));
}

async function cloud(main: HTMLElement) {
  const [pods, prices] = await Promise.all([api.pods(), api.prices()]);
  const podCard = (p: Pod) => {
    const mins = Math.round((Date.now() - p.started_at) / 60_000);
    if (p.state === "stopped") {
      return h("div", { class: "card" },
        h("div", { class: "row", style: "justify-content:space-between" }, h("h2", {}, `${p.name} on ${p.gpu}`), h("span", { class: "chip" }, "Stopped")),
        h("p", { class: "sub", style: "margin:8px 0 0" }, `No GPU billing. Its disk still bills ${money(p.volume_cost_per_hour)} an hour; ${money(p.spent)} spent so far.`));
    }
    return h("div", { class: "card hero" },
      h("div", { class: "row", style: "justify-content:space-between" }, h("h2", {}, `${p.name} on ${p.gpu}`), h("span", { class: "chip", style: "color:var(--live);border-color:var(--live)" }, p.state === "starting" ? "Starting" : `Running ${mins} min`)),
      h("div", { class: "row", style: "gap:28px;margin:14px 0" },
        h("div", {}, h("div", { class: "k" }, "Spent so far"), h("div", { class: "cost" }, money(p.spent))),
        h("div", {}, h("div", { class: "k" }, "Rate"), h("div", { class: "cost", style: "font-size:20px" }, money(p.cost_per_hour), h("small", { class: "k", style: "font-size:12px" }, " /hour"))),
        h("div", {}, h("div", { class: "k" }, "Session cap"), h("div", { class: "cost", style: "font-size:20px" }, money(p.cap)))),
      h("div", { class: "bar" }, h("i", { style: `width:${Math.min(100, (100 * p.spent) / p.cap)}%;background:var(--gold)` })),
      h("p", { class: "k", style: "margin:8px 0 12px" }, `Stops by itself after ${p.idle_stop_minutes} minutes with no requests (idle now: ${p.idle_minutes} min), at the ${money(p.cap)} cap, or if Fonsviva loses touch with it.`),
      h("div", { class: "row" }, h("button", { class: "btn primary", onclick: act(() => main, async () => { await api.stopPod(p.id); render(); }) }, "Stop now"),
        h("span", { class: "k" }, `Stopping ends GPU billing. Its disk keeps billing ${money(p.volume_cost_per_hour)} an hour until you delete the pod.`)));
  };
  main.append(
    h("div", {}, h("h1", {}, "RunPod"), h("p", { class: "sub" }, "Your own RunPod account. Personal use only; client work never leaves this computer.")),
    h("div", { class: "grid2" },
      pods.length ? h("div", { style: "display:grid;gap:14px;align-content:start" }, ...pods.map(podCard)) : h("div", { class: "card" }, h("h2", {}, "No pods"), h("p", { class: "sub" }, "Nothing is billing right now.")),
      h("div", { class: "card" }, h("h2", { style: "margin-bottom:10px" }, "Prices before you start"),
        h("table", {}, h("thead", {}, h("tr", {}, h("th", {}, "GPU"), h("th", { class: "r" }, "Per hour"), h("th", { class: "r" }, "2 hours"))),
          h("tbody", {}, ...prices.map((p) => h("tr", {}, h("td", {}, p.gpu), h("td", { class: "r mono" }, money(p.per_hour)), h("td", { class: "r mono" }, money(p.per_hour * 2)))))),
        h("p", { class: "k", style: "margin:10px 0 0" }, "Read from RunPod just now. Your key stays in this computer's keychain."))));
}

function settings(main: HTMLElement) {
  main.append(h("h1", {}, "Settings"), h("p", { class: "sub" }, "Channel, data folder, cloud and keys."));
}

const SETUP_DONE = "fonsviva.setup-done";
function remember(k: string) {
  try { localStorage.setItem(k, "1"); } catch { /* storage off: onboarding shows again, which is harmless */ }
}
function recall(k: string) {
  try { return localStorage.getItem(k) === "1"; } catch { return false; }
}

async function boot() {
  try {
    const c = await connect();
    api = c.api;
    sample = c.sample;
    const [have, run] = await Promise.all([api.models(), api.running()]);
    hw = await api.hardware();
    if (!run.length && !(have.length && recall(SETUP_DONE))) await onboarding();
    else render();
  } catch (e) {
    app.replaceChildren(h("main", {}, h("h1", {}, "Fonsviva could not start"), say(e)));
  }
}

boot();
