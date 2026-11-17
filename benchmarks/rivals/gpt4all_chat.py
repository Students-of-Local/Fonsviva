"""GPT4All Python SDK first answer, run inside the rival container.

Usage: python chat.py MODEL_NAME PROMPT MAX_TOKENS
The docs' example is GPT4All("<name>.gguf") then model.generate(...). The
harness downloads the model first, so the constructor here only loads it.
Streams the answer to time the first token. Prints one JSON object.
"""

import json
import sys
import time

from gpt4all import GPT4All

name, prompt, max_tokens = sys.argv[1], sys.argv[2], int(sys.argv[3])
t_load0 = time.time()
model = GPT4All(name)
t_load1 = time.time()
first = None
pieces = []
with model.chat_session():
    t_send = time.time()
    for piece in model.generate(prompt, max_tokens=max_tokens, temp=0, streaming=True):
        if first is None and piece.strip():
            first = time.time()
        pieces.append(piece)
t_end = time.time()
n = len(pieces)
print(json.dumps({
    "t_load0": t_load0,
    "t_load1": t_load1,
    "t_send": t_send,
    "t_first_content": first,
    "ttft_ms": round((first - t_send) * 1000, 1) if first else None,
    "total_ms": round((t_end - t_send) * 1000, 1),
    "completion_tokens": n,
    "gen_tps": round((n - 1) / (t_end - first), 2) if first and n > 1 and t_end > first else None,
    "answer": "".join(pieces),
    "n_ctx": getattr(getattr(model, "model", None), "n_ctx", None),
    "device": getattr(model, "device", None),
    "ok": first is not None,
}))
