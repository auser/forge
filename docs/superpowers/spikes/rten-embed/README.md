# Spike: rten-backed local embedder

Throwaway harness behind §7 of
`../specs/2026-10-30-local-inference-runtime-exploration.md`. It is not part
of the forge workspace and is never built by CI.

It loads a published `bge-small-en-v1.5` ONNX encoder under `rten` 0.27.0 and
reports model-load time, single-text and batched latency, embedding
dimensions, and cosine similarity as a correctness check.

## Run

```sh
cargo build --release

mkdir -p models
B=https://huggingface.co/Xenova/bge-small-en-v1.5/resolve/main
curl -sL -o models/model.onnx "$B/onnx/model.onnx"
curl -sL -o models/model_quantized.onnx "$B/onnx/model_quantized.onnx"
curl -sL -o models/tokenizer.json "$B/tokenizer.json"

./target/release/rten-spike models/model.onnx models/tokenizer.json
```

## `.rten` comparison (optional)

The official ONNX -> `.rten` converter is a Python tool, not a Rust binary:

```sh
python3 -m venv .venv && .venv/bin/pip install rten-convert
.venv/bin/rten-convert models/model.onnx models/model.rten
./target/release/rten-spike models/model.rten models/tokenizer.json
```

Measured result: same file size, same load time, same peak RSS, same latency.
`.rten` was not worth converting for this model.
