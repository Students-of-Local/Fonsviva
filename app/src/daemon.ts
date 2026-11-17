// Finds the daemon. Inside the app, the Tauri side attaches to or starts `fonsviva serve` and hands
// over its address and tokens. In a plain browser there is no daemon, so the mock answers.

import { HttpApi, type Api, type ApiError, type Session } from "./api";
import { MockApi } from "./mock";

export async function connect(): Promise<{ api: Api; sample: boolean }> {
  if (!("__TAURI_INTERNALS__" in window)) return { api: new MockApi(), sample: true };
  const { invoke } = await import("@tauri-apps/api/core");
  // The Tauri side rejects with a plain-words string; turn it into an ApiError the window shows.
  const attach = () =>
    invoke<Session>("daemon_info").catch((e: unknown): never => {
      throw { code: "daemon", message: String(e) } satisfies ApiError;
    });
  return { api: new HttpApi(await attach(), attach), sample: false };
}
