// Starts a model download and waits for it to finish, without the two ways a wait can hang:
// a "done" sent before the event stream opened, and a stream that fails or ends. The pull
// starts only once the stream is listening, so even an instant failure is heard. Pull events
// that arrive before pull() names our job wait in `early`, so another client's download never
// finishes or redraws this one.

import type { Api, ApiError, Job } from "./api";

export async function pullAndWait(api: Api, id: string, onJob: (j: Job) => void): Promise<void> {
  let job = "";
  const early: Job[] = [];
  let finish!: (e?: unknown) => void;
  let opened!: () => void;
  const open = new Promise<void>((res) => (opened = res));
  const done = new Promise<void>((res, rej) => (finish = (e) => (e === undefined ? res() : rej(e))));
  const seen = (j: Job) => {
    if (j.kind === "pull" && j.id !== job) return;
    onJob(j);
    if (j.kind !== "pull") return;
    if (j.state === "done") finish();
    if (j.state === "failed") finish({ code: "pull", message: j.message ?? "The download failed." } satisfies ApiError);
  };
  // Subscribe first, so no event from the pull is missed.
  const off = api.events(
    (kind, data) => {
      if (kind !== "job") return;
      const j = data as Job;
      if (j.kind === "pull" && !job) early.push(j);
      else seen(j);
    },
    (e) => finish(e),
    () => opened(),
  );
  try {
    await Promise.race([open, done]);
    ({ job } = await api.pull(id));
    for (const j of early.splice(0)) seen(j);
    // A pull that was already on disk, or finished first, may never send another event.
    if ((await api.models()).some((m) => m.id === id)) finish();
    await done;
  } finally {
    off();
  }
}
