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
  wireApply(target: WireTarget["target"]): Promise<WireTarget>;
  wireRevert(target: WireTarget["target"]): Promise<WireTarget>;
  pods(): Promise<Pod[]>;
  prices(): Promise<GpuPrice[]>;
  stopPod(id: string): Promise<void>;
  ask(model: string, prompt: string, onToken: (t: string) => void): Promise<Stats>;
  events(on: (kind: string, data: unknown) => void): () => void;
}

export class HttpApi implements Api {
  constructor(
    private base: string,
    private token: string,
    private appKey: string,
  ) {}

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(this.base + path, {
      method,
      headers: {
        authorization: `Bearer ${this.token}`,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
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
  wireApply(t: WireTarget["target"]) { return this.req<WireTarget>("POST", `/api/v1/wire/${t}`); }
  wireRevert(t: WireTarget["target"]) { return this.req<WireTarget>("DELETE", `/api/v1/wire/${t}`); }
  pods() { return this.req<Pod[]>("GET", "/api/v1/cloud/runpod/pods"); }
  prices() { return this.req<GpuPrice[]>("GET", "/api/v1/cloud/runpod/templates"); }
  stopPod(id: string) { return this.req<void>("POST", `/api/v1/cloud/runpod/pods/${id}/stop`); }

  async ask(model: string, prompt: string, onToken: (t: string) => void): Promise<Stats> {
    const res = await fetch(this.base + "/v1/chat/completions", {
      method: "POST",
      headers: { authorization: `Bearer ${this.appKey}`, "content-type": "application/json" },
      body: JSON.stringify({ model, stream: true, messages: [{ role: "user", content: prompt }] }),
    });
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
        const delta = JSON.parse(line.slice(5)).choices?.[0]?.delta?.content;
        if (delta) onToken(delta);
      }
    }
    const all = await this.stats();
    return all.find((s) => s.model === model) ?? all[0];
  }

  events(on: (kind: string, data: unknown) => void): () => void {
    const ctrl = new AbortController();
    (async () => {
      const res = await fetch(this.base + "/api/v1/events", {
        headers: { authorization: `Bearer ${this.token}` },
        signal: ctrl.signal,
      });
      if (!res.body) return;
      const reader = res.body.getReader();
      const dec = new TextDecoder();
      let buf = "";
      let kind = "message";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        let i;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          if (line.startsWith("event:")) kind = line.slice(6).trim();
          else if (line.startsWith("data:")) on(kind, JSON.parse(line.slice(5)));
          else if (line === "") kind = "message";
        }
      }
    })().catch(() => {});
    return () => ctrl.abort();
  }
}
