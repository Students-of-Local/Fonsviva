import { afterEach, expect, test } from "bun:test";
import { HttpApi, type Session } from "../src/api";

const A: Session = { base_url: "http://127.0.0.1:7341", owner_token: "owner-A", app_key: "key-A" };
const B: Session = { base_url: "http://127.0.0.1:7341", owner_token: "owner-B", app_key: "key-B" };
const real = globalThis.fetch;
afterEach(() => (globalThis.fetch = real));

const tick = () => new Promise((r) => setTimeout(r, 5));

/**
 * A daemon stand-in. `/api/v1/events` stays open until `die()`; every other request records the
 * bearer it carried. `down` makes requests fail the way fetch does when nothing listens.
 */
function daemon() {
  const sent: string[] = [];
  let close = () => {};
  const st = { down: false, die: () => close() };
  globalThis.fetch = (async (url: string, init: RequestInit = {}) => {
    const auth = new Headers(init.headers).get("authorization") ?? "";
    if (init.signal?.aborted) throw new DOMException("aborted", "AbortError");
    if (st.down) throw new TypeError("connection refused");
    if (url.endsWith("/api/v1/events")) {
      let open = true;
      const body = new ReadableStream({ start: (c) => (close = () => open && ((open = false), c.close())) });
      init.signal?.addEventListener("abort", () => close());
      return new Response(body, { status: 200 });
    }
    sent.push(auth);
    return Response.json([]);
  }) as typeof fetch;
  return { sent, st };
}

test("after the daemon exits, no token goes out until the proof runs again", async () => {
  const { sent, st } = daemon();
  let proofs = 0;
  const api = new HttpApi(A, async () => (proofs++, B));
  await api.models();
  expect(sent).toEqual(["Bearer owner-A"]);
  // The daemon exits; a stranger now answers on the same port and would take any request.
  st.die();
  await tick();
  await api.models();
  expect(proofs).toBe(1);
  expect(sent).toEqual(["Bearer owner-A", "Bearer owner-B"]);
});

test("requests that fail together share one proof", async () => {
  const { st } = daemon();
  let proofs = 0;
  const api = new HttpApi(A, async () => {
    proofs++;
    st.down = false;
    await tick();
    return B;
  });
  st.down = true;
  await Promise.all([api.models(), api.running(), api.wire()]);
  expect(proofs).toBe(1);
});

test("a proof that fails is tried again on the next request, not skipped", async () => {
  const { sent, st } = daemon();
  let proofs = 0;
  const api = new HttpApi(A, async () => {
    if (++proofs === 1) throw { code: "daemon", message: "not yet" };
    return B;
  });
  st.die();
  await tick();
  await expect(api.models()).rejects.toMatchObject({ code: "daemon" });
  await api.models();
  expect(proofs).toBe(2);
  expect(sent).toEqual(["Bearer owner-B"]);
});

test("closing an event stream is not a lost daemon", async () => {
  daemon();
  let proofs = 0;
  const api = new HttpApi(A, async () => (proofs++, B));
  const off = api.events(() => {});
  off();
  await tick();
  await api.models();
  expect(proofs).toBe(0);
});
