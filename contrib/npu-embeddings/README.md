# NPU embeddings service

A tiny OpenAI-compatible `/v1/embeddings` endpoint that runs a small embedding model on the
**Intel NPU** (via OpenVINO), so Systema's knowledge base gets semantic search without taking GPU
away from titan-engine. The GPU keeps doing generation; the otherwise-idle NPU does embeddings.

## Why
`kb_search` uses BM25 (keyword) retrieval by default. Point Systema at this service and it switches
to **vector (semantic) search** — `kb_add` embeds each chunk here, `kb_search` embeds the query and
ranks by cosine. Turn it off and the KB falls back to BM25 with nothing extra running.

## Requirements
- An Intel NPU. On this box: Core Ultra 9 285HX, `/dev/accel/accel0`, `intel_vpu` driver (check
  with `ls /dev/accel` and `lsmod | grep intel_vpu`).
- Python 3.10+.

## Run
```sh
cd contrib/npu-embeddings
python -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt
python serve.py --model BAAI/bge-small-en-v1.5 --device NPU --port 1300
```
First run exports the model to OpenVINO IR (cached in `./ov-cache`). Use `--device CPU` to validate
the Systema integration without NPU drivers, then switch to `--device NPU`.

## Wire it into Systema
```toml
# ~/.systema/systema.toml
[inference]
embed_endpoint = "http://127.0.0.1:1300/v1"
embed_model    = "bge-small-en-v1.5"
```
Restart the node. New `kb_add`s are embedded; existing BM25-only entries keep working (re-add them
to embed, or add a `kb reindex` later). Verify end to end:
```sh
systema kb-add --title "note" "titan-engine serves models on port 1234"
systema ask operator "search the knowledge base for which port the model server uses"
```

## Notes
- The NPU wants **static shapes**, so inputs are padded/truncated to `--max-seq` (default 512).
- `bge-small-en-v1.5` is 384-dim and fast; swap in any sentence-embedding model that OpenVINO can
  export (e5-small, all-MiniLM-L6-v2, …). Keep one model id per KB so vectors stay comparable.
