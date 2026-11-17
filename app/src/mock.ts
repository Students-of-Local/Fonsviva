// A daemon stand-in for the browser and demo mode. Every number is labeled sample data in the window.

import type { Api, ApiError, CatalogEntry, InstalledModel, Pod, Running, Stats, WireTarget } from "./api";

const GB = 1e9;
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

export class MockApi implements Api {
  private installed: InstalledModel[] = [
    { id: "ollama:lfm2.5:1.2b", name: "LFM2.5 1.2B Instruct", source: "ollama", bytes: 0.73 * GB, quant: "Q4_K_M", context: 32768, fit: "fits", fit_reason: "Uses 1.1 GB of 15 GB free", measured_tok_s: 118 },
  ];
  private loaded: Running[] = [];
  private wires: WireTarget[] = [
    { target: "pi", state: "absent", file: "~/.pi/agent/models.json" },
    { target: "t3code", state: "absent", file: "~/.t3/userdata/settings.json" },
    { target: "eukhe", state: "absent", detail: "Local Studio" },
    { target: "openai", state: "applied", base_url: "http://127.0.0.1:7341/v1" },
  ];
  private listeners: ((kind: string, data: unknown) => void)[] = [];
  private startedAt = Date.now() - 38 * 60_000;

  private emit(kind: string, data: unknown) {
    for (const l of this.listeners) l(kind, data);
  }

  async hardware() {
    return {
      os: "macOS 26", arch: "arm64", cpu: { name: "Apple M5", cores: 10 },
      ram_total_bytes: 24 * GB, ram_free_bytes: 15 * GB, unified_memory: true,
      gpus: [{ vendor: "Apple", name: "M5 GPU", vram_bytes: 24 * GB, backend: "metal" as const }],
      disk_free_bytes: 212 * GB,
    };
  }

  async catalog(): Promise<CatalogEntry[]> {
    return [
      { id: "qwen3-4b-instruct", name: "Qwen3 4B Instruct", summary: "Good at writing, questions and simple code.", quant: "Q4_K_M", download_bytes: 2.5 * GB, memory_bytes: 3.1 * GB, context: 32768, fit: "fits", fit_reason: "Uses 3.1 GB of your 15 GB free", license: "Apache 2.0", est_tok_s: 45, recommended: true },
      { id: "lfm2.5-1.2b-instruct", name: "LFM2.5 1.2B Instruct", summary: "Very fast, light, good for short tasks.", quant: "Q4_K_M", download_bytes: 0.73 * GB, memory_bytes: 1.1 * GB, context: 32768, fit: "fits", fit_reason: "Uses 1.1 GB of your 15 GB free", license: "LFM Open", est_tok_s: 118, installed_as: "ollama:lfm2.5:1.2b" },
      { id: "qwen3-coder-8b", name: "Qwen3 Coder 8B", summary: "Best local coding model that fits here.", quant: "Q4_K_M", download_bytes: 5.0 * GB, memory_bytes: 7.4 * GB, context: 32768, fit: "tight", fit_reason: "Uses 7.4 GB of 15 GB free; close other apps", license: "Apache 2.0", est_tok_s: 24 },
      { id: "gemma-4-12b", name: "Gemma 4 12B", summary: "Strong all-round model.", quant: "Q4_K_M", download_bytes: 7.3 * GB, memory_bytes: 10.2 * GB, context: 16384, fit: "tight", fit_reason: "Fits only at 16k context", license: "Gemma", est_tok_s: 14 },
      { id: "llama-4-scout", name: "Llama 4 Scout", summary: "Large model for hard problems.", quant: "Q4_K_M", download_bytes: 65 * GB, memory_bytes: 68 * GB, context: 32768, fit: "no", fit_reason: "Needs 68 GB, you have 24", license: "Llama 4" },
    ];
  }

  async models() { return [...this.installed]; }
  async running() { return [...this.loaded]; }

  async stats(): Promise<Stats[]> {
    return this.loaded.map((r) => ({ model: r.model, ttft_ms: 182, prompt_tok_s: 612, gen_tok_s: 46.3, memory_bytes: r.memory_bytes }));
  }

  async pull(id: string) {
    const entry = (await this.catalog()).find((c) => c.id === id)!;
    const job = "job-" + id;
    (async () => {
      this.emit("job", { id: "engine", kind: "engine", label: "Engine (llama.cpp, Metal build, 18 MB)", state: "done" });
      for (let p = 0; p <= 100; p += 4) {
        this.emit("job", { id: job, kind: "pull", label: "Model download", state: "running", done_bytes: (entry.download_bytes * p) / 100, total_bytes: entry.download_bytes, seconds_left: Math.round((100 - p) * 0.66) });
        await wait(70);
      }
      this.installed.unshift({ id, name: entry.name, source: "fonsviva", bytes: entry.download_bytes, quant: entry.quant, context: entry.context, fit: entry.fit, fit_reason: entry.fit_reason });
      this.emit("job", { id: job, kind: "pull", label: "Model download", state: "done" });
    })();
    return { job };
  }

  async run(model: string): Promise<Running> {
    await wait(600);
    const m = this.installed.find((i) => i.id === model)!;
    const r: Running = { model, name: m.name, engine: "llama.cpp", backend: "Metal", context: m.context, gpu_layers: 36, total_layers: 36, memory_bytes: 3.1 * GB };
    this.loaded = [r];
    this.emit("run", r);
    return r;
  }

  async stop(model: string) { this.loaded = this.loaded.filter((r) => r.model !== model); this.emit("run", null); }
  async wire() { return [...this.wires]; }

  async wireApply(target: WireTarget["target"], _confirm?: boolean) {
    await wait(250);
    const w = this.wires.find((x) => x.target === target)!;
    w.state = "applied";
    return { ...w };
  }

  async wireRevert(target: WireTarget["target"], _confirm?: boolean) {
    await wait(250);
    const w = this.wires.find((x) => x.target === target)!;
    w.state = "absent";
    return { ...w };
  }

  async pods(): Promise<Pod[]> {
    const mins = (Date.now() - this.startedAt) / 60_000;
    return [{ id: "pod-1", name: "Llama 4 Scout", gpu: "1 x A100 80 GB", state: "running", started_at: this.startedAt, cost_per_hour: 1.64, volume_cost_per_hour: 0.03, spent: (1.64 * mins) / 60, cap: 5, idle_minutes: 3, idle_stop_minutes: 15 }];
  }

  async prices() {
    return [
      { gpu: "RTX 4090 24 GB", per_hour: 0.69 },
      { gpu: "L40S 48 GB", per_hour: 0.86 },
      { gpu: "A100 80 GB", per_hour: 1.64 },
    ];
  }

  async stopPod() { this.startedAt = Date.now(); }

  async ask(model: string, _prompt: string, onToken: (t: string) => void): Promise<Stats | undefined> {
    await wait(182);
    for (const w of "Red, yellow and blue in paint; red, green and blue in light.".split(/(?<= )/)) {
      onToken(w);
      await wait(22);
    }
    return (await this.stats()).find((s) => s.model === model);
  }

  events(on: (kind: string, data: unknown) => void, _onError?: (e: ApiError) => void, onOpen?: () => void) {
    this.listeners.push(on);
    onOpen?.();
    return () => { this.listeners = this.listeners.filter((l) => l !== on); };
  }
}
