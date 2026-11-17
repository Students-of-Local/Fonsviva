import { expect, test } from "bun:test";
import type { Api, ApiError, InstalledModel, Job } from "../src/api";
import { pullAndWait } from "../src/pull";

type On = (kind: string, data: unknown) => void;

/** A daemon stand-in whose pull behaves as each case needs. */
function fake(opts: { onPull?: (emit: On, fail: (e: ApiError) => void) => void; installed?: string[]; openAfter?: number }) {
  let on: On = () => {};
  let fail: (e: ApiError) => void = () => {};
  const state = { subscribed: false, unsubscribed: false };
  const api = {
    events(cb: On, onError?: (e: ApiError) => void, onOpen?: () => void) {
      on = cb;
      fail = onError ?? (() => {});
      setTimeout(() => {
        state.subscribed = true;
        onOpen?.();
      }, opts.openAfter ?? 0);
      return () => (state.unsubscribed = true);
    },
    async pull() {
      if (!state.subscribed) throw new Error("pull before subscribe");
      opts.onPull?.(on, fail);
      return { job: "j1" };
    },
    async models() {
      return (opts.installed ?? []).map((id) => ({ id }) as InstalledModel);
    },
  } as unknown as Api;
  return { api, state };
}

const within = <T>(p: Promise<T>, ms = 300) =>
  Promise.race([p, new Promise<T>((_, rej) => setTimeout(() => rej(new Error("hung")), ms))]);

const job = (state: Job["state"], extra: Partial<Job> = {}): Job => ({ id: "j1", kind: "pull", label: "Model download", state, ...extra });

test("a pull that finishes before pull() returns is not missed", async () => {
  const { api, state } = fake({ onPull: (emit) => emit("job", job("done")) });
  await within(pullAndWait(api, "m", () => {}));
  expect(state.unsubscribed).toBe(true);
});

test("a model already on disk resolves with no events at all", async () => {
  const { api } = fake({ installed: ["m"] });
  await within(pullAndWait(api, "m", () => {}));
});

test("a stream that fails rejects instead of hanging", async () => {
  const { api, state } = fake({ onPull: (_, fail) => setTimeout(() => fail({ code: "events", message: "lost" }), 5) });
  await expect(within(pullAndWait(api, "m", () => {}))).rejects.toMatchObject({ code: "events" });
  expect(state.unsubscribed).toBe(true);
});

test("a failed job rejects with its message", async () => {
  const { api } = fake({ onPull: (emit) => setTimeout(() => emit("job", job("failed", { message: "disk full" })), 5) });
  await expect(within(pullAndWait(api, "m", () => {}))).rejects.toMatchObject({ code: "pull", message: "disk full" });
});

test("progress reaches the screen and another job's done does not finish this one", async () => {
  const seen: string[] = [];
  const { api } = fake({
    onPull: (emit) =>
      setTimeout(() => {
        emit("job", job("running", { done_bytes: 1 }));
        emit("job", job("done", { id: "other" }));
        emit("job", job("done"));
      }, 5),
  });
  await within(pullAndWait(api, "m", (j) => seen.push(`${j.id}:${j.state}`)));
  expect(seen).toEqual(["j1:running", "j1:done"]);
});

test("another client's done that lands before pull() returns does not finish this one", async () => {
  const seen: string[] = [];
  let emit: (kind: string, data: unknown) => void = () => {};
  const { api } = fake({
    onPull: (e) => {
      emit = e;
      e("job", job("done", { id: "other" }));
    },
  });
  const wait = pullAndWait(api, "m", (j) => seen.push(`${j.id}:${j.state}`));
  await new Promise((r) => setTimeout(r, 20));
  expect(seen).toEqual([]);
  emit("job", job("done"));
  await within(wait);
  expect(seen).toEqual(["j1:done"]);
});

test("the pull waits until the stream is listening, so an instant failure is heard", async () => {
  const { api } = fake({ openAfter: 20, onPull: (emit) => emit("job", job("failed", { message: "offline" })) });
  await expect(within(pullAndWait(api, "m", () => {}))).rejects.toMatchObject({ message: "offline" });
});
