# webgpu-community/s1-mini-webgpu vs our sherpa-onnx pipeline, head-to-head

Studied + benchmarked 2026-08-28 on this machine (Snapdragon X Plus X1P42100, 8×Oryon @ 3.24 GHz, Balanced, aarch64, Windows 11). External reference: https://huggingface.co/spaces/webml-community/s1-mini-webgpu (static Space, all logic inlined in `index.html`). We replicated their full pipeline locally on CPU to benchmark it in the same harness/audio/metrics as ours.

## What the Space is

A two-stage, fully in-browser pipeline:

1. Moonshine base (`onnx-community/moonshine-base-ONNX`) as ASR (`device: "webgpu"` in the Space source): `onnx/encoder_model_quantized.onnx` (19.6 MB) + `onnx/decoder_model_merged_quantized.onnx` (40.5 MB) ≈ 60 MB.
2. s1-mini (`onnx-community/s1-mini-ONNX`) as a text normalizer / prose rewriter (Qwen3-0.6B Superwhisper fine-tune, thinking disabled): `onnx/model_q4f16.onnx` (0.3 MB graph) + `model_q4f16.onnx_data` (338.7 MB) ≈ 339 MB.

Runtime = Transformers.js 4.2.0 → onnxruntime-web WebGPU EP, no CPU fallback, main-thread inference. Flow: mic → AudioWorklet PCM → resample 16 kHz → whole clip transcribed in one call (no partials while speaking) → TextStreamer token-streams the rewriter's cleaned output.

## Our setup

Sherpa-ONNX 1.13.5, Nemotron Speech Streaming 0.6B int8 80 ms tier, streaming transducer, greedy, CPU 2 threads, 632 MB single artifact. `feed_audio` per chunk → Live-mode partials; `finalize()` → verbatim transcript pasted.

## Logical comparison

| Axis | Space (s1-mini-webgpu) | Us (Amanuensis) |
|---|---|---|
| ASR stage | Moonshine base ~170M, non-streaming | Nemotron 0.6B streaming 80 ms tier |
| Post-pass | s1-mini q4f16 (0.6B) rewrite, thinking off | none, verbatim text |
| Compute | WebGPU GPU, no CPU fallback | native CPU, 2–4 threads |
| Partials while speaking | none | yes (Live mode) |
| Download | ~60 + ~339 MB ≈ 400 MB (2 repos) | 632 MB (1 repo) |
| Runtime | browser sandbox (main thread) | native worker thread |
| Works offline/headless | no | yes |

## How we stood up their pipeline locally (same CPU, same audio)

- Their ASR → `src/bin/asr_bench_offline.rs` (new): sherpa-onnx `OfflineRecognizer` + `OfflineMoonshineModelConfig` (v1 export) on the same native onnxruntime CPU as our Nemotron → metrics case-by-case identical to `asr_bench.rs`. Artifact: `sherpa-onnx-moonshine-base-en-int8.tar.bz2` (from k2-fsa GitHub release `asr-models`, 239 MB download / ~272 MB disk: `preprocess.onnx`, `encode.int8.onnx`, `uncached_decode.int8.onnx`, `cached_decode.int8.onnx`, `tokens.txt`). Same base weights as the Space's `onnx-community/moonshine-base-ONNX` (sherpa re-export; sherpa can't load the optimum-form ONNX directly, fair stand-in, same architecture/quant family).
- Their normalizer → `bench/run_normalizer.ps1` (new): llama.cpp win-arm64 prebuilt (built b10675) + first-party `superwhisper/s1-mini-GGUF/s1-mini-q4_k_m.gguf` (462 MB). Vendor-blessed thinking-disable: `--jinja --chat-template-kwargs '{"enable_thinking":false}' --temp 0 -t 8 -c 2048` + the Space's verbatim system prompt and default control line `[Styling: semi-formal] [Structure: prose] [Context: general]`.

## Benchmarks: ASR stage only (2 threads, release, same 4 clips)

| | Nemotron 0.6B int8 (ours) | Moonshine base int8 (theirs) |
|---|---:|---:|
| cold load | 2.3–2.4 s | 1.3–1.6 s |
| resident after load | 720 MiB | 281–301 MiB |
| resident after a session | 743–746 MiB | 519–656 MiB |
| clip 1 (24.7 s) decode | 24.93 s · 1.01x | 1.02 s · 0.04x |
| clip 2 (15.2 s) decode | 14.57 s · 0.96x | 0.46 s · 0.03x |
| clip 3 (33.9 s) decode | 33.85 s · 1.00x | 2.73 s · 0.08x |
| clip 4 (25.0 s) decode | 24.67 s · 0.99x | 2.14 s · 0.09x |
| streaming partials | yes (38/13/56/49 advances) | no (full-clip at stop) |

Moonshine is ~11–25x cheaper to run than our streaming tier and uses ~60% less RAM. (Published moonshine base RTF on x86-64 CPU ~0.07–0.16; this ARM64 CPU measures 0.03–0.09. Nemotron's published ~7% WER tier needs ~1.0x on this chip.)

## Benchmarks: full pipeline, time-to-text per clip

| Clip | Ours: stream during speech + finalize | Theirs: transcribe at stop + s1-mini rewrite |
|---|---|---:|
| 1 | 0.67 s finalize (decode ran live) | 1.02 + 4.00 = 5.02 s |
| 2 | 0.60 s | 0.46 + 3.44 = 3.90 s |
| 3 | 0.58 s | 2.73 + 4.91 = 7.64 s |
| 4 | 0.48 s | 2.14 + 4.88 = 7.02 s |

Their tail latency is ~4–8 s post-stop; ours ~0.5–0.7 s (because decode happens during recording, but that steady running decode sits right at the realtime edge on live mode, see thread scaling below). s1-mini numbers include per-invocation model load (a persistent server amortizes it; tokens/sec part is ~2–4 s).

## Transcripts (same 4 clips)

`1`, Ours: "Does this all currently work as I want you to make sure you go through every change in the code base as soon as possible so we get clear understanding of what have we done" · Moonshine: "Does this all currently work? As I want you to make sure you go through every change in the codebase as soon as possible. So, we get clear understanding of what have we done." → s1-mini: unchanged (already clean).
`2`, Ours: "Me some files have you check them out yet make sure you do" (missed a word) · Moonshine: "The joys and me some files have you checked them out yet make sure you do" (caught the verb, plus a "the joys" hallucination on the leading noise) → s1-mini: "The joys and me, some files have you checked them out yet? Make sure you do."
`3`, both garbled the same hard section; s1-mini only fixed punctuation.
`4`, Ours: "…it's fine but but are you reacting to this you follow some…" · Moonshine: "…yeah it's fine but why are you right so this you follow some…" → s1-mini: *"David said to me, "Is this correct?" I don't think so, but why don't you correct me? I don't know. And then I said to him that, yeah, it's fine, but why are you right? So this you follow some as vividly as you are."*. Clearly the best-presented result, adding punctuation/casing its two ASRs lacked.

## Thread scaling (ours; feed=480 = 30 ms)

| Threads | RTF range | avg ms/chunk | max ms/chunk |
|---|---|---|---|
| 1 | 1.15–1.19x (drops audio) | 34.6–35.8 | 201.8 |
| 2 (default) | 0.96–1.01x | 28.8–30.2 | 145.7 |
| 4 | 0.84–0.87x | 25.3–26.0 | 138.8 |

## Verdict

- Compute / RAM / accuracy of the ASR stage: theirs wins decisively. Moonshine is 11–25x cheaper (0.03–0.09 RTF vs ~1.0), loads in 1.5 s, sits at ~300 MiB, and transcribes these hard casual clips at least as well (punctuated, better verb retention). Our streaming 0.6B tier is near its realtime ceiling on this Snapdragon.
- Live UX: ours wins by design. We show partials while speaking and finalize in ~0.6 s; their pipeline is silent until you stop, then costs ~4–8 s (transcribe + LLM rewrite) before text appears. They cannot stream at all.
- Final-text polish: theirs wins. s1-mini genuinely cleans disfluencies/formatting (clip 4), at the cost of a second ~0.4–0.5 GB model and ~3–5 s per clip. It cannot fix ASR word errors it never heard (clip 3).
- Takeaway for Amanuensis: the cost/quality of moonshine as a record-mode ASR (or a cheap second model) is attractive, and a small local normalizer pass over `finalize()` text would close our biggest quality gap without a second heavyweight runtime. Keep streaming Nemotron where partials matter (live mode).

## Ground-truth sizes

| Artifact | role | size |
|---|---|---|
| moonshine `encoder_model_quantized.onnx` + `decoder_model_merged_quantized.onnx` | their ASR (optimum export) | 19.6 + 40.5 MB |
| moonshine base int8 v1 tar (sherpa export, what we benched) | their ASR, local | 239 MB tar / ~272 MB disk |
| s1-mini `model_q4f16.onnx` + `.onnx_data` | their normalizer | 0.3 + 338.7 MB |
| `superwhisper/s1-mini-GGUF` `s1-mini-q4_k_m.gguf` | their normalizer, local | 462 MB |
| Nemotron int8 80 ms (encoder/decoder/joiner/tokens) | ours | 632 MB / 632 MiB |

## Experiment log: GPU probes (2026-08-29)

Vulkan/DML on the ONNX side is a dead end on this build (evidence below). `src/bin/asr_gpu_probe.rs` calls sherpa-onnx with each provider on both models. Every non-`cpu` provider prints a fallback warning and runs on CPU:

```
C:\a\sherpa-onnx\sherpa-onnx\csrc\provider.cc:StringToProvider:35 Unsupported string: dml. Fallback to cpu
```

(ditto `vulkan`, `cuda`, `coreml`, `openvino`) and onnxruntime then reports `Only the CPUExecutionProvider is available... -DSHERPA_ONNX_ENABLE_GPU=ON`. Wall times across all providers ≈ CPU, e.g. nemotron online "dml" ran 58.44 s = 2.36x realtime, but that pass was CPU-throttled again; the prior cool pass was 24.93 s = 1.01x, so the probe's timing proves only "same speed as CPU". Reasons: the official win-arm64 sherpa lib is compiled CPU-only; onnxruntime has no Vulkan EP for Windows (its WebGPU EP maps to D3D12/DirectML); DirectML EP is not compiled in and there is no DML arm64 artifact. Real GPU ONNX on this box = rebuilding sherpa/onnxruntime with DML from source (multi-day) or running in a browser via WebGPU→D3D12.

The LLM side does hit the GPU. llama.cpp publishes an Adreno-targeted OpenCL win-arm64 build (`llama-b10675-bin-win-opencl-adreno-arm64.zip`, 12.4 MB). It reports `ggml_opencl: device: 'Qualcomm(R) Adreno(TM) X1-45 GPU (OpenCL 3.0 ...)'`. Same s1-mini `q4_k_m` GGUF, `llama-bench`, 8 threads, pp64/tg64:

| backend | pp64 (t/s) | tg64 (t/s) |
|---|---:|---:|
| CPU (`-ngl 0`) | 370.0 | 45.1 |
| GPU (`-ngl 99`) | 482.5 | 30.5 |

GPU is +30% on prompt-eval but −32% on generation. A 0.6B model is memory-bound, and 8×Oryon beats the Adreno at token generation.

End-to-end s1-mini rewrite, real 4 clips: same `run_normalizer.ps1` (now takes `-GpuLayers`), same OpenCL binary, `-ngl 0` vs `-ngl 99`, greedy; 8/8 runs returned byte-identical cleaned text (deterministic):

| clip | CPU (ms) | GPU OpenCL (ms) | GPU slower |
|---|---:|---:|---:|
| 1 | 5403 | 14555 | 2.7x |
| 2 | 3802 | 13060 | 3.4x |
| 3 | 5497 | 14657 | 2.7x |
| 4 | 5533 | 13569 | 2.5x |

GPU end-to-end is 2.5–3.4x slower per clip. Conclusion: for our exact workload, a 0.6B LLM and mobile-optimal ONNX ASR on Snapdragon, the Oryon CPU legitimately beats the Adreno iGPU; the GPU only pays off on long prompt prefill, which never happens at transcript scale. The Space's in-browser WebGPU is thus not an unconditional speed win on this class of hardware; it wins on footprint and portability, not raw speed.

## Out of scope / future

- Streaming ASR partials directly into the user's textbox is a future feature request (noted, out of scope). Current product only shows partials in the recording UI and pastes the verbatim transcript on stop.

## Files

- `src/bin/asr_bench_offline.rs`, native Moonshine benchmark (mirrors `asr_bench.rs`; arg 1 = model dir, then raw files, `--threads=N`).
- `src/bin/asr_gpu_probe.rs`, sherpa provider probe (`<moonshine_dir> <nemotron_dir> <raw16k-f32le> [--threads=N]`); logs every provider incl. the CPU fallback warnings.
- `bench/run_normalizer.ps1`, s1-mini rewrite bench (`-Transcript`, times + peak RSS + cleaned text; `-Json` for machine output; `-GpuLayers` for GPU offload).

Notes: s1-mini peak RSS reads low (~25 MB) because llama.cpp memory-maps the GGUF in a short-lived process; a persistent server (2K ctx) is expected ~0.7–0.9 GB steady-state per llama.cpp docs, not the mmap peak. First benchmark batch on this Snapdragon was CPU-throttled (~2.6x skew, one clip ran >290 s); all numbers above were re-measured in cool, single-load passes. Sources: Space `index.html`, HF API trees, k2-fsa sherpa-onnx docs (`models.html`, `models-v2.html`, `rust-api-examples/moonshine_v2.rs`), superwhisper/s1-mini-GGUF card, repo `research/model-artifacts.md`, `research/sherpa-onnx-streaming.md`.