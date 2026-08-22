# Plan: parakeet-ptt → Rust GPUI dictation app

Port of `C:\Users\Yashraj\Downloads\parakeet-ptt` (Bun + Python) to a single Rust binary.
Research backing this plan: [research/sherpa-onnx-streaming.md](../research/sherpa-onnx-streaming.md), [research/parakeet-rs-fallback.md](../research/parakeet-rs-fallback.md).

## Unchanged from the original app

Audio capture (`cpal`), hotkeys (`global-hotkey`), paste (`arboard` + `enigo`), HUD (`GPUI`), model fetching (`hf-hub`), phase state machine (loading → idle → recording → transcribing), pending-start queue during load, `MIN_AUDIO_SECS` gate, filler-word cleanup, `asr_s`/`load_s` timing events, toggle-F9 semantics.

## ASR layer

### Two models, split by dictation mode

| Mode | Model | Chunks | Why |
|---|---|---|---|
| record | `nvidia/parakeet-unified-en-0.6b` | 240 ms | Nothing visible mid-recording; only final accuracy + how fast the last chunk resolves after stop. 240 ms is the top of its accuracy range before right-context degradation (NVIDIA's own cutoff). |
| live | `nvidia/nemotron-speech-streaming-en-0.6b` | 80 ms | Genuinely stateful streaming (cache-aware FastConformer-RNNT, not buffer-replay); text must track voice closely. WER worse (~7.2–7.8%) than unified@240ms — correct trade when partials are visible. |

Reference numbers: Parakeet-TDT-0.6B-v2 scores 6.05% avg WER but only fake-streams on sherpa-onnx (issue #2918 — workaround was re-decoding a growing buffer). That's what we're moving off of for both modes.

int8 download sizes, verified from HF file trees + embedded ONNX metadata ([research/model-artifacts.md](../research/model-artifacts.md)): both core payloads ≈ **632 MiB** — unified 663,048,739 B, nemotron 661,919,414 B (compressed tarballs: 501 MB vs 464 MB). Both well under Parakeet TDT's 2.3 GB fp32 encoder. Note: the parakeet-rs fallback artifact (bobNight) uses a different file layout → backends cannot share downloads. **Resident RAM after load is unpublished anywhere** — measure locally in phase 3 before trusting `ModelSpec.min_ram_gb`.

Validation precedent: OpenWhispr shipped int8 Nemotron through a local sherpa-onnx streaming path with live partials ([1.7.6] – 2026-07-18, #1131/#1238).

### Backend: `sherpa_onnx` crate (primary), `parakeet-rs` (fallback impl)

No hand-written TDT/transducer decode loop anywhere — we call a wrapper, we don't write one.

**Resolved open question** (was: "does sherpa_onnx expose chunk-size presets as a runtime option?"):
No runtime knob exists in either crate. Chunk/latency tier is baked into each exported ONNX tarball in **both** crates — selection happens by choosing which model files to download, not by config value. Since the literal fallback condition fires identically for both, the deciding criteria become correctness, artifacts, and Windows friction:

- `sherpa_onnx` 1.13.x passes all three:
  - Parakeet-unified got real buffered-RNNT streaming in PR #3575 (May 2026), auto-dispatched via encoder ONNX metadata (`streaming_model_type=nemo_parakeet_unified_streaming`); export presets include exactly our 240 ms tier. Issue-tracker pass came back conditionally clean ([research/sherpa-lifecycle-and-issues.md](../research/sherpa-lifecycle-and-issues.md)) — **pin sherpa-onnx ≥ 1.13.5** (the two accuracy fixes #3785/#3857 exist only there; ideally 1.13.6).
  - Exact artifact exists: HF `csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms`.
  - Nemotron ships as per-tier int8 tarballs (80/160/560/1120 ms) — same `OnlineRecognizer::from_transducer` API for both models.
  - Windows: static linking by default, prebuilt lib auto-downloaded at build time unless `SHERPA_ONNX_LIB_DIR` pinned; CPU-only avoids CUDA DLL pitfalls (#878). Caveats: ~25% documented (lean on repo examples), build-time network fetch, MSVC toolchain required.
- `parakeet-rs` v0.3.7 (Jul 2026) is an unusually strong second implementation behind the trait: pure-Rust via `ort`, wraps BOTH targets natively (`ParakeetUnified`, `Nemotron::transcribe_chunk`, cache-aware EOU variant), DirectML GPU path on Windows, healthy PR intake. Single-maintainer risk noted.

Decision: implement `AsrBackend` once against `sherpa_onnx`; keep the trait clean enough that a `parakeet-rs` backend is a drop-in swap if decoding issues appear.

### Shared trait, model-agnostic

```rust
trait AsrBackend {
    fn feed_audio(&mut self, samples: &[f32]);
    fn partial(&self) -> Option<String>;
    fn finalize(&mut self) -> String;
}
```

Implemented once against sherpa-onnx, parameterized by config — not once per model:

```rust
struct ModelSpec {
    id: &'static str,
    display_name: &'static str,
    hf_repo: &'static str,
    size_mb: u32,
    min_ram_gb: u32,
    chunk_ms: Option<u32>,   // None = offline/batch only
    wer_note: &'static str,
}
```

Mode-to-model is a lookup, not a branch: record → unified@240ms, live → nemotron@80ms. Adding a model later or user overrides = new registry entry, not new inference code.

### Session lifecycle (F9 mapping — decided)

From [research/sherpa-lifecycle-and-issues.md](../research/sherpa-lifecycle-and-issues.md):

- **Fresh `create_stream()` per F9 press in both modes.** Stream creation is cheap (no model reload) and a fully clean slate — sidesteps every `reset()` trap (feature buffer kept, zipformer ys-context retained, LSTM residue).
- **Record**: pump `accept_waveform`/`is_ready`/`decode` during hold → on release push ~0.6 s zero tail padding (covers right-context chunk; OpenWhispr's proven constant) → `input_finished()` → drain → take text → drop stream.
- **Live**: long-lived stream during hold, partials from `get_result().text`; on release identical tail-pad → drain → commit. No mid-hold resets for normal dictations (segment bookkeeping across resets is the top cause of duplicated/dropped text); explicit endpointing only for very long holds (rule3 ≈ 20 s) — and set the rules explicitly, Rust config defaults are 0/0/0 unlike C++'s 2.4/1.2/20.
- `decoding_method = greedy_search` — beam search + hotwords unsupported on this path (#3572 open).

### Memory residency across mode switches (proposal)

Load/unload per mode switch: only the active mode's model resident at a time. Rationale: the app lives in the background permanently, mode flips are rare, reload after first fetch is fast via OS page cache, and the pending-start queue already absorbs load latency. Consequence: the onboarding RAM check validates against **one** model's footprint plus headroom, not the sum of both. Prerequisite BEFORE locking this design: phase 3 must measure actual RSS post-load **and** unload→reload wall time for both models — neither number is published anywhere, and record mode's instant-paste pitch depends on reload being fast when F9 lands right after a mode switch.

## Additions

### Boundary redecode (record mode, post-v1)

Nothing has been shown mid-recording, and short buffers decode near-instantly → redecode the last chunk(s) / last couple seconds with full surrounding context before pasting, instead of pasting the streaming pass's output for that chunk. Chunk boundaries are where streaming ASR loses accuracy most; record mode fixes it for free since there's no visible latency cost. The existing longest-suffix merge logic already stitches chunk boundaries — this is one extra correction pass on the final chunk only.

### Onboarding & model switching

First launch opens an onboarding window, not idle:
- Read device RAM + core count at startup (`sysinfo` crate), match against `ModelSpec.min_ram_gb`, preselect best-fit as recommended option.
- Registry listed as alternatives, each with its own download button through `hf-hub`, progress callback into GPUI.
- Choice stored per-mode in config under app config dir; skip onboarding once config exists.
- Download-in-progress hooks into the existing pending-start queue the same way model-load-in-progress does today.

## Build order

1. ✅ **Done** — Pipeline spine: cpal capture thread ([src/audio.rs](../src/audio.rs), mono downmix, RMS @30 ms windows over mpsc) → GPUI live 26-bar waveform with running-peak normalization ([src/main.rs](../src/main.rs)); `cargo check` clean.
2. ✅ **Done** — Hotkeys + FSM: global-hotkey 0.8 F9 toggle (`GlobalHotKeyManager` created on GPUI's main/win32-pump thread, kept alive via `mem::forget`; forwarder thread passes only `Pressed` events), `Phase {Loading, Idle, Recording, Transcribing}` × `Mode {Record, Live}` FSM, clickable mode chip, waveform bars gated to `Recording`. Note: global-hotkey already reports key release on Windows → push-to-talk later needs no new plumbing.
3. ASR live mode: Nemotron via sherpa_onnx @80ms chunks (`feature_dim=128`, greedy_search), partials into HUD. Checklist: ☐ confirm `normalize_type` is read from encoder metadata, not left at a Rust-side default (nemotron expects none) · ☐ log resident RSS after load **and** unload→reload wall time for BOTH models — gates locking the load/unload-per-switch residency design.
4. ASR record mode: unified @240ms chunks (`feature_dim=128`, per_feature normalization), same trait, final-paste path. Checklist: ☐ confirm `normalize_type=per_feature` is honored from encoder metadata · ☐ test a multi-minute hold (~5-min paragraph): fresh-stream-per-press does NOT cap internal stream-state growth (feature buffer, encoder cache, segment bookkeeping) — if degradation/memory growth appears, apply live mode's ~20 s endpoint-flush threshold here too.
5. Paste path: arboard + enigo, gates, filler cleanup.
6. Boundary redecode (record mode).
7. Onboarding window: device detection, recommended model, download UX, per-mode config; RAM check validates single-resident-model footprint.
8. Polish: transparent capsule window.
9. Tuning: latency benchmarks vs old Python baseline, WER spot-check on own voice samples — including the acknowledged +1–3% int8 residual penalty (#3782).

## Risks & known constraints

- int8 Nemotron carries an **acknowledged residual +1–3% WER vs fp32 NeMo** (cross-chunk LSTM corruption was fixed in v1.13.5, but quantization penalty remains) — inherent, benchmark in phase 9.
- Rust crate gaps: ~25% documented; `OnlineRecognizer::create` returns `Option` with no error string; `reset_encoder` and `EndpointRule.must_contain_nonsilence` not exposed; endpoint-rule defaults are 0/0/0 and must be set explicitly.
- Silent-numerics regressions ride bundled ONNX Runtime bumps (#3791) → pin exact sherpa-onnx versions; smoke-test our exact model pair against the pinned binaries before locking.
- Open Windows bugs in the broader NeMo family (offline TDT empty decode #3767, non-ASCII path misdecode #3885) — different subsystems, watch only.
- GPUI transparency / always-on-top fields don't appear in any example at our pinned rev — expect to dig into `WindowOptions` source directly in phase 8.
