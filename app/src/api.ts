// Client for the Fonsviva daemon API (docs/API.md). The window draws only what this returns.

export type Fit = "fits" | "tight" | "no";

export interface Gpu {
  vendor: string;
  name: string;
  vram_bytes: number;
  backend: "metal" | "cuda" | "rocm" | "vulkan" | "cpu";
}

export interface Hardware {
  os: string;
  arch: string;
  cpu: { name: string; cores: number };
  ram_total_bytes: number;
  ram_free_bytes: number;
  unified_memory: boolean;
  gpus: Gpu[];
  disk_free_bytes: number;
}

export interface CatalogEntry {
  id: string;
  name: string;
  summary: string;
  quant: string;
  download_bytes: number;
  memory_bytes: number;
  context: number;
  fit: Fit;
  fit_reason: string;
  license: string;
  est_tok_s?: number;
  recommended?: boolean;
  /** Id of the installed model this entry already is, such as the same weights in Ollama. */
  installed_as?: string;
}

export interface InstalledModel {
  id: string;
  name: string;
  source: "fonsviva" | "ollama" | "lmstudio" | "hf";
  bytes: number;
  quant: string;
  context: number;
  fit: Fit;
  fit_reason: string;
  measured_tok_s?: number;
}

export interface Running {
  model: string;
  name: string;
  engine: "llama.cpp" | "ollama";
  backend: string;
  context: number;
  gpu_layers: number;
  total_layers: number;
  memory_bytes: number;
}

export interface Stats {
  model: string;
  ttft_ms: number;
  prompt_tok_s: number;
  gen_tok_s: number;
  memory_bytes: number;
}

export type WireState = "absent" | "applied" | "drifted" | "unavailable";

export interface WireTarget {
  target: "pi" | "t3code" | "eukhe" | "openai";
  state: WireState;
  file?: string;
  detail?: string;
  base_url?: string;
}

export interface Pod {
  id: string;
  name: string;
  gpu: string;
  state: "starting" | "running" | "stopped";
  started_at: number;
  cost_per_hour: number;
  /** What the pod's disk bills per hour while the pod is stopped. */
  volume_cost_per_hour: number;
  spent: number;
  cap: number;
  idle_minutes: number;
  idle_stop_minutes: number;
}

export interface GpuPrice {
  gpu: string;
  per_hour: number;
}

export interface Template {
  id: string;
  name: string;
  gpus: GpuPrice[];
}

export interface Job {
  id: string;
  kind: "pull" | "engine" | "load" | "bench";
  label: string;
  done_bytes?: number;
  total_bytes?: number;
  state: "queued" | "running" | "done" | "failed";
  seconds_left?: number;
  message?: string;
}

export interface ApiError {
  code: string;
  message: string;
  fix?: string;
}

export interface Api {
  hardware(): Promise<Hardware>;
  catalog(): Promise<CatalogEntry[]>;
  models(): Promise<InstalledModel[]>;
  running(): Promise<Running[]>;
  stats(): Promise<Stats[]>;
  pull(id: string): Promise<{ job: string }>;
  run(model: string): Promise<Running>;
  stop(model: string): Promise<void>;
  wire(): Promise<WireTarget[]>;
  /** `confirm` goes ahead on a `drifted` target after saving a second backup. */
  wireApply(target: WireTarget["target"], confirm?: boolean): Promise<WireTarget>;
  wireRevert(target: WireTarget["target"], confirm?: boolean): Promise<WireTarget>;
  pods(): Promise<Pod[]>;
  prices(): Promise<GpuPrice[]>;
  stopPod(id: string): Promise<void>;
  ask(model: string, prompt: string, onToken: (t: string) => void): Promise<Stats>;
  /** `onError` runs once if the stream cannot open or ends. */
  events(on: (kind: string, data: unknown) => void, onError?: (e: ApiError) => void): () => void;
}

export interface Session {
  base_url: string;
  owner_token: string;
  app_key: string;
}

export class HttpApi implements Api {
  /**
   * `reattach` runs the daemon proof again. It is called whenever the daemon stops answering, so
   * tokens never go to a port until the process there proves it owns this data dir.
   */
  constructor(
    private s: Session,
    private reattach: () => Promise<Session>,
  ) {}

  private async send(path: string, init: (s: Session) => RequestInit): Promise<Response> {
    try {
      return await fetch(this.s.base_url + path, init(this.s));
    } catch {
      this.s = await this.reattach();
      return fetch(this.s.base_url + path, init(this.s));
    }
  }

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await this.send(path, (s) => ({
      method,
      headers: {
        authorization: `Bearer ${s.owner_token}`,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    }));
    if (!res.ok) {
      const e = (await res.json().catch(() => null)) as { error?: ApiError } | null;
      throw e?.error ?? { code: String(res.status), message: res.statusText };
    }
    return res.status === 204 ? (undefined as T) : ((await res.json()) as T);
  }

  hardware() { return this.req<Hardware>("GET", "/api/v1/hardware"); }
  catalog() { return this.req<CatalogEntry[]>("GET", "/api/v1/catalog"); }
  models() { return this.req<InstalledModel[]>("GET", "/api/v1/models"); }
  running() { return this.req<Running[]>("GET", "/api/v1/running"); }
  stats() { return this.req<Stats[]>("GET", "/api/v1/stats"); }
  pull(id: string) { return this.req<{ job: string }>("POST", "/api/v1/models/pull", { id }); }
  run(model: string) { return this.req<Running>("POST", "/api/v1/run", { model }); }
  stop(model: string) { return this.req<void>("POST", "/api/v1/stop", { model }); }
  wire() { return this.req<WireTarget[]>("GET", "/api/v1/wire"); }
  wireApply(t: WireTarget["target"], confirm = false) { return this.req<WireTarget>("POST", `/api/v1/wire/${t}`, confirm ? { confirm } : undefined); }
  wireRevert(t: WireTarget["target"], confirm = false) { return this.req<WireTarget>("DELETE", `/api/v1/wire/${t}`, confirm ? { confirm } : undefined); }
  pods() { return this.req<Pod[]>("GET", "/api/v1/cloud/runpod/pods"); }
  async prices() {
    const t = await this.req<Template[]>("GET", "/api/v1/cloud/runpod/templates");
    const best = new Map<string, number>();
    for (const g of t.flatMap((x) => x.gpus)) best.set(g.gpu, Math.min(best.get(g.gpu) ?? Infinity, g.per_hour));
    return [...best].map(([gpu, per_hour]) => ({ gpu, per_hour }));
  }
  stopPod(id: string) { return this.req<void>("POST", `/api/v1/cloud/runpod/pods/${id}/stop`); }

  async ask(model: string, prompt: string, onToken: (t: string) => void): Promise<Stats> {
    const res = await this.send("/v1/chat/completions", (s) => ({
      method: "POST",
      headers: { authorization: `Bearer ${s.app_key}`, "content-type": "application/json" },
      body: JSON.stringify({ model, stream: true, messages: [{ role: "user", content: prompt }] }),
    }));
    if (!res.ok || !res.body) throw { code: String(res.status), message: "The model did not answer." };
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!line.startsWith("data:") || line === "data: [DONE]") continue;
        const delta = parseJson(line.slice(5))?.choices?.[0]?.delta?.content;
        if (delta) onToken(delta);
      }
    }
    const all = await this.stats();
    return all.find((s) => s.model === model) ?? all[0];
  }

  events(on: (kind: string, data: unknown) => void, onError?: (e: ApiError) => void): () => void {
    const ctrl = new AbortController();
    const lost: ApiError = { code: "events", message: "Fonsviva stopped sending updates.", fix: "Check that the Fonsviva engine is still running, then try again." };
    (async () => {
      const res = await this.send("/api/v1/events", (s) => ({
        headers: { authorization: `Bearer ${s.owner_token}` },
        signal: ctrl.signal,
      }));
      if (!res.ok || !res.body) throw lost;
      const parser = sse(on);
      const reader = res.body.getReader();
      const dec = new TextDecoder();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        parser(dec.decode(value, { stream: true }));
      }
      throw lost;
    })().catch((e) => {
      if (!ctrl.signal.aborted) onError?.(e?.code ? e : lost);
    });
    return () => ctrl.abort();
  }
}

function parseJson(text: string): any {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * A server-sent events parser: CR, LF or CRLF line ends, multi-line `data`, comments ignored.
 * A frame whose data is not JSON is skipped, so one bad frame never ends the stream.
 */
export function sse(on: (kind: string, data: unknown) => void): (chunk: string) => void {
  let buf = "";
  let kind = "message";
  let data: string[] = [];
  return (chunk) => {
    buf += chunk;
    const lines = buf.split(/\r\n|\r|\n/);
    buf = lines.pop() ?? "";
    for (const line of lines) {
      if (line === "") {
        if (data.length) {
          const v = parseJson(data.join("\n"));
          if (v !== undefined) on(kind, v);
        }
        kind = "message";
        data = [];
      } else if (line.startsWith("event:")) kind = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
  };
}
