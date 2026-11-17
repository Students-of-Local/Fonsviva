"""Stream one chat completion and time it.

Usage: python -I ttft.py URL MODEL PROMPT MAX_TOKENS [BEARER_TOKEN]

URL is an OpenAI-style chat completions endpoint. Prints one JSON object:
the send time, time to the first content token, time to the first token of
any kind (reasoning included), generation speed and the answer text.
"ok" means some token came back; "answered" means visible content did.
Standard library only, so it runs in a stock python image.
"""

import http.client
import json
import sys
import time
import urllib.parse


def main():
    url, model, prompt, max_tokens = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    token = sys.argv[5] if len(sys.argv) > 5 else ""
    u = urllib.parse.urlsplit(url)
    body = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": True,
        "stream_options": {"include_usage": True},
    })
    headers = {"Content-Type": "application/json", "Accept": "text/event-stream"}
    if token:
        headers["Authorization"] = "Bearer " + token

    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=600)
    conn.connect()
    t_send = time.time()
    conn.request("POST", u.path, body=body, headers=headers)
    resp = conn.getresponse()
    if resp.status != 200:
        print(json.dumps({"ok": False, "status": resp.status, "body": resp.read().decode("utf-8", "replace")[:2000]}))
        return 1

    t_first_any = t_first_content = t_last = None
    content, reasoning, chunks, usage = [], [], 0, None
    for raw in resp:
        line = raw.decode("utf-8", "replace").strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            break
        try:
            obj = json.loads(data)
        except ValueError:
            continue
        if obj.get("usage"):
            usage = obj["usage"]
        for choice in obj.get("choices") or []:
            delta = choice.get("delta") or {}
            text = delta.get("content") or ""
            think = delta.get("reasoning_content") or delta.get("reasoning") or ""
            now = time.time()
            if (text or think) and t_first_any is None:
                t_first_any = now
            if text:
                if t_first_content is None:
                    t_first_content = now
                content.append(text)
                chunks += 1
                t_last = now
            if think:
                reasoning.append(think)
                t_last = now
    t_end = time.time()
    conn.close()

    tokens = (usage or {}).get("completion_tokens") or chunks
    gen_tps = None
    if t_first_any and t_last and t_last > t_first_any and tokens > 1:
        gen_tps = round((tokens - 1) / (t_last - t_first_any), 2)
    print(json.dumps({
        "ok": t_first_any is not None,
        "answered": t_first_content is not None,
        "t_send": t_send,
        "t_first_content": t_first_content,
        "t_first_any": t_first_any,
        "ttft_ms": round((t_first_content - t_send) * 1000, 1) if t_first_content else None,
        "first_any_ms": round((t_first_any - t_send) * 1000, 1) if t_first_any else None,
        "total_ms": round((t_end - t_send) * 1000, 1),
        "completion_tokens": tokens,
        "usage_reported": usage is not None,
        "gen_tps": gen_tps,
        "answer": "".join(content),
        "reasoning_chars": len("".join(reasoning)),
    }))
    return 0


if __name__ == "__main__":
    sys.exit(main())
