// Finds the daemon. Inside the app, the Tauri side attaches to or starts `fonsviva serve` and hands
// over its address and tokens. In a plain browser there is no daemon, so the mock answers.

import { HttpApi, type Api } from "./api";
import { MockApi } from "./mock";

interface DaemonInfo {
  base_url: string;
  owner_token: string;
  app_key: string;
}

export async function connect(): Promise<{ api: Api; sample: boolean }> {
  if (!("__TAURI_INTERNALS__" in window)) return { api: new MockApi(), sample: true };
  const { invoke } = await import("@tauri-apps/api/core");
  const info = await invoke<DaemonInfo>("daemon_info");
  return { api: new HttpApi(info.base_url, info.owner_token, info.app_key), sample: false };
}
