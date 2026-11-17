import { expect, test } from "bun:test";
import { sse } from "../src/api";

function feed(...chunks: string[]) {
  const got: [string, unknown][] = [];
  const push = sse((kind, data) => got.push([kind, data]));
  for (const c of chunks) push(c);
  return got;
}

test("LF frames with an event name", () => {
  expect(feed('event: job\ndata: {"id":"a"}\n\n')).toEqual([["job", { id: "a" }]]);
});

test("CRLF line ends reset the event name", () => {
  expect(feed('event: job\r\ndata: {"id":"a"}\r\n\r\ndata: {"id":"b"}\r\n\r\n')).toEqual([
    ["job", { id: "a" }],
    ["message", { id: "b" }],
  ]);
});

test("a frame split across chunks", () => {
  expect(feed("event: run\nda", 'ta: {"m":1', "}\n", "\n")).toEqual([["run", { m: 1 }]]);
});

test("a bad frame is skipped and the stream goes on", () => {
  expect(feed(": keep-alive\n\ndata: not json\n\ndata: {\"ok\":true}\n\n")).toEqual([["message", { ok: true }]]);
});

test("multi-line data joins with newlines", () => {
  expect(feed('data: {"a":\ndata: 1}\n\n')).toEqual([["message", { a: 1 }]]);
});

test("a CRLF split between chunks keeps the event name", () => {
  expect(feed("event: job\r", '\ndata: {"id":"a"}\r\n\r\n')).toEqual([["job", { id: "a" }]]);
});
