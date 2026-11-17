// Starts a model download and waits for it to finish, without the two ways a wait can hang:
// a "done" sent before the event stream opened, and a stream that fails or ends.

import type { Api, ApiError, Job } from "./api";

export async function pullAndWait(api: Api, id: string, onJob: (j: Job) => void): Promise<void> {
  let job = "";
  let finish!: (e?: unknown) => void;
  const done = new Promise<void>((res, rej) => (finish = (e) => (e === undefined ? res() : rej(e))));
  // Subscribe first, so no event from the pull is missed.
  const off = api.events(
    (kind, data) => {
      if (kind !== "job") return;
      const j = data as Job;
      onJob(j);
      if (j.kind !== "pull" || (job && j.id !== job)) return;
      if (j.state === "done") finish();
      if (j.state === "failed") finish({ code: "pull", message: j.message ?? "The download failed." } satisfies ApiError);
    },
    (e) => finish(e),
  );
  try {
    ({ job } = await api.pull(id));
    // A pull that was already on disk, or finished first, may never send another event.
    if ((await api.models()).some((m) => m.id === id)) finish();
    await done;
  } finally {
    off();
  }
}
