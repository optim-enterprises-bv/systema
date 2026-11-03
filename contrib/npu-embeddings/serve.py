#!/usr/bin/env python3
"""A tiny OpenAI-compatible /v1/embeddings service for Systema's knowledge base.

It runs a small embedding model on the Intel NPU via OpenVINO (optimum-intel), so embeddings
never contend with titan-engine for the GPU. Point Systema at it:

    # ~/.systema/systema.toml
    [inference]
    embed_endpoint = "http://127.0.0.1:1300/v1"
    embed_model    = "bge-small-en-v1.5"

Then `kb_add` embeds chunks here and `kb_search` does semantic search; with this off, the KB
falls back to BM25 automatically.

Usage:
    pip install -r requirements.txt
    python serve.py --model BAAI/bge-small-en-v1.5 --device NPU --port 1300
    # --device CPU to validate the integration without NPU drivers; GPU also works.

Notes:
- The Intel NPU wants static shapes, so inputs are padded/truncated to --max-seq (default 512).
- First run exports the model to OpenVINO IR and caches it under ./ov-cache.
"""
import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import torch
from optimum.intel import OVModelForFeatureExtraction
from transformers import AutoTokenizer

ARGS = None
TOKENIZER = None
MODEL = None


def load(model_id: str, device: str, max_seq: int):
    global TOKENIZER, MODEL
    TOKENIZER = AutoTokenizer.from_pretrained(model_id)
    # export=True converts to OpenVINO IR on first run; ov_config caches the compiled blob.
    MODEL = OVModelForFeatureExtraction.from_pretrained(
        model_id, export=True, device=device, ov_config={"CACHE_DIR": "./ov-cache"}
    )
    # Static shape for the NPU: fixed batch 1 and sequence length.
    try:
        MODEL.reshape(1, max_seq)
        MODEL.compile()
    except Exception as e:  # noqa: BLE001 - CPU/GPU tolerate dynamic shapes; log and continue.
        print(f"(reshape skipped: {e})")


def embed(texts: list[str]) -> list[list[float]]:
    out = []
    for text in texts:
        enc = TOKENIZER(
            text, return_tensors="pt", truncation=True, padding="max_length", max_length=ARGS.max_seq
        )
        with torch.no_grad():
            res = MODEL(**enc)
        # Mean-pool the last hidden state over real tokens, then L2-normalize.
        hidden = res.last_hidden_state  # (1, seq, dim)
        mask = enc["attention_mask"].unsqueeze(-1).type_as(hidden)
        summed = (hidden * mask).sum(1)
        counts = mask.sum(1).clamp(min=1e-9)
        vec = (summed / counts).squeeze(0).cpu().numpy().astype(np.float32)
        norm = np.linalg.norm(vec)
        if norm > 0:
            vec = vec / norm
        out.append(vec.tolist())
    return out


class Handler(BaseHTTPRequestHandler):
    def _send(self, code: int, obj: dict):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 - health / models
        if self.path.rstrip("/").endswith("/models"):
            self._send(200, {"object": "list", "data": [{"id": ARGS.model, "object": "model"}]})
        else:
            self._send(200, {"status": "ok"})

    def do_POST(self):  # noqa: N802
        if not self.path.rstrip("/").endswith("/embeddings"):
            return self._send(404, {"error": "not found"})
        length = int(self.headers.get("Content-Length", 0))
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            return self._send(400, {"error": "bad json"})
        inp = req.get("input", [])
        texts = [inp] if isinstance(inp, str) else list(inp)
        if not texts:
            return self._send(400, {"error": "no input"})
        vecs = embed(texts)
        data = [{"object": "embedding", "index": i, "embedding": v} for i, v in enumerate(vecs)]
        self._send(200, {"object": "list", "model": req.get("model", ARGS.model), "data": data})

    def log_message(self, *_):  # quiet
        pass


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--model", default="BAAI/bge-small-en-v1.5")
    p.add_argument("--device", default="NPU", help="NPU, CPU or GPU")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=1300)
    p.add_argument("--max-seq", type=int, default=512)
    ARGS = p.parse_args()
    print(f"loading {ARGS.model} on {ARGS.device} (max_seq={ARGS.max_seq}) …")
    load(ARGS.model, ARGS.device, ARGS.max_seq)
    print(f"embeddings service on http://{ARGS.host}:{ARGS.port}/v1/embeddings")
    ThreadingHTTPServer((ARGS.host, ARGS.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
