# sherpa-onnx Rust crate: streaming ASR research

Researched 2026-08-22. Web-research only; no code changes.
Question: can the `sherpa_onnx` crate drive **parakeet-unified @240ms chunks** (record mode) and **nemotron @80ms chunks** (live mode) on Windows?

## TL;DR verdict

**Yes, feasible.** Both models run through the same `OnlineRecognizer` API; chunk size is baked into each model tarball, so you pick latency by downloading the right archive and pointing two recognizer instances at it. Caveats: the Rust wrapper is thinly documented (~25%), the build downloads native libs from GitHub releases at compile time unless pinned, and GPU/CUDA on Windows has known DLL pitfalls. No evidence of firewall prompts when linking in-process (static by default).

---

## 1. Crate identity & versioning

From [docs.rs/sherpa-onnx](https://docs.rs/sherpa-onnx/latest/sherpa_onnx/):

- Current: **sherpa-onnx 1.13.5** (published **12 Aug 2026**), Apache-2.0, repo [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), owner `csukuangfj`.
- Deps: `serde`, `serde_json`, `sherpa-onnx-sys ^1.13.5`.
- **Only ~25% of the crate is documented** — plan to read `examples/` in the repo as primary API reference.
- `sherpa-onnx-sys` 1.13.5 (11 Aug 2026) has build-deps `bzip2`, `tar`, `ureq` → its build script downloads/extracts native archives.

## 2. Streaming API shape (Rust)

All verified on docs.rs for 1.13.5:

```rust
// sherpa_onnx_sys::online_asr (repr(C))
FeatureConfig { sample_rate: i32, feature_dim: i32 }

// sherpa_onnx::OnlineTransducerModelConfig (online_asr.rs#50-54)
OnlineTransducerModelConfig {
    encoder: Option<String>,
    decoder: Option<String>,
    joiner: Option<String>,
}

// sherpa_onnx::OnlineRecognizerConfig
OnlineRecognizerConfig {
    feat_config: FeatureConfig,
    model_config: OnlineModelConfig,
    decoding_method: Option<String>,   // "greedy_search" | "modified_beam_search"
    max_active_paths: i32,
    enable_endpoint: bool,
    rule1_min_trailing_silence: f32,
    rule2_min_trailing_silence: f32,
    rule3_min_utterance_length: f32,
    hotwords_file: Option<String>,
    hotwords_score: f32,
    hotwords_buf: Option<String>,
    ctc_fst_decoder_config: ...,
    rule_fsts / rule_fars: Option<String>,
    blank_penalty: f32,
    hr: HomophoneReplacerConfig,
}
```

Usage pattern (from repo examples + discussion #3735):

```rust
let mut config = OnlineRecognizerConfig::default();
config.model_config.transducer.encoder = Some("...encoder.int8.onnx".into());
config.model_config.transducer.decoder = Some("...decoder.int8.onnx".into());
config.model_config.transducer.joiner  = Some("...joiner.int8.onnx".into());
config.model_config.tokens = Some("...tokens.txt".into());
let recognizer = OnlineRecognizer::from_transducer(&config)?; // factory method
```

Key finding: **there is no chunk-size / latency / buffer knob anywhere in the config structs.** Endpoint detection (`rule1/rule2/rule3`) is how you detect end-of-utterance in record mode (`IsEndpoint()` → grab final text → `Reset()`).

## 3. Chunk size is chosen per-tarball, not per-config

From [k2-fsa docs: nemotron-streaming](https://github.com/k2-fsa/sherpa/blob/master/docs/source/onnx/nemo/nemotron-streaming.rst):

- Nemotron streaming ships as **separate archives per latency tier**, chunk baked in at export time:
  - `sherpa-onnx-nemotron-speech-streaming-en-0.6b-{80,160,560,1120}ms-int8-2026-04-25.tar.bz2`
  - multilingual `nemotron-3.5-asr-streaming-0.6b` with the same four tiers
- CLI/runtime just points `--encoder/--decoder/--joiner/--tokens` at the chosen directory. Docs state: *"The larger the chunk size, the higher the accuracy."*
- So: **two recognizers = two tarball sets.** Record mode loads the parakeet-unified 240ms set; live mode loads the nemotron 80ms set. Nothing to configure beyond file paths.

## 4. Parakeet-unified streaming support

- **Issue [#2918](https://github.com/k2-fsa/sherpa-onnx/issues/2918)** confirmed real and open (created 2025-12-21): before proper support, users did *fake* streaming — re-decode a growing buffer every tick, which gets slower as the buffer grows. Maintainer pointed people at simulate-streaming APKs.
- **PR [#3575](https://github.com/k2-fsa/sherpa-onnx/pull/3575)** merged **May 9 2026**: adds a buffered RNNT streaming path for Parakeet Unified.
  - Dispatch is **automatic via encoder ONNX metadata** (`streaming_model_type=buffered_nemo_rnnt`) — no new fields on `OnlineModelConfig`.
  - Reads `buffer_{left,chunk,right}_{feature,encoder}_frames`, `subsampling_factor`, `pred_rnn_layers`, etc. from metadata.
  - Export script supports latency presets **1120ms / 560ms / 240ms**.
- Official artifacts exist:
  - GitHub release asset: `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-560ms.tar.bz2`
  - HuggingFace (maintainer): [`csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms`](https://huggingface.co/csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms) ← the exact variant this project needs for record mode
- Discussion [#3735](https://github.com/k2-fsa/sherpa-onnx/discussions/3735) confirms true-streaming usage via `OnlineRecognizer::from_transducer`.
- NVIDIA model card notes: buffered streaming only, min latency **160ms**, range 160–2080ms in 80ms steps; NVIDIA itself recommends nemotron-speech-streaming for lowest latency — consistent with our split (parakeet@240ms record / nemotron@80ms live).
- Version note: PR landed May 9 2026; PyPI shipped 1.13.2 on May 13 → **any v1.13.x includes it**; current 1.13.5 definitely does. v1.12.x does not.

## 5. Windows story

- **Static linking is the default.** If `SHERPA_ONNX_LIB_DIR` is unset, `-sys`'s build script auto-downloads the matching prebuilt `-lib` archive from GitHub releases (e.g. `sherpa-onnx-v1.13.5-win-x64-static-MT-Release-lib.tar.bz2`). A `shared` feature copies DLLs next to your binaries instead.
- Implications:
  - Deployment is simple — no onnxruntime.dll hunting at runtime.
  - **Build-time network dependency**: `cargo build` fetches ~100MB+ from github.com unless you pin `SHERPA_ONNX_LIB_DIR` or vendor. Worth noting for CI reproducibility.
  - MSVC toolchain required (VS 2022 for building C++; MinGW unsupported).
- Known pitfall if enabling CUDA provider: issue [#878](https://github.com/k2-fsa/sherpa-onnx/issues/878) — `LoadLibrary` error 126 when `onnxruntime_providers_cuda.dll` (+ cuDNN deps) aren't findable. CPU-only dictation avoids this class entirely.
- **Firewall prompts**: no direct reports found tying sherpa-onnx to Windows Defender firewall popups. The plausible trigger would be shelling out to `sherpa-onnx-*-websocket-server.exe` (inbound TCP listener). With the default static/in-process linking there's no sidecar socket → no prompt expected. (Unverified either way — flagging honestly.)

## Open items

- Exact field names on `OnlineModelConfig`'s sibling configs (`paraformer`, `nemo_ctc`, …) not individually quoted here — irrelevant for transducer-based parakeet/nemotron use.
- If we later want GPU: verify onnxruntime CUDA EP DLL layout against #878 before shipping an installer.
