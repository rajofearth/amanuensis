# parakeet-rs fallback research

Researched 2026-08-22 via web (docs.rs, GitHub). Fallback candidate for the ASR layer if `sherpa_onnx` hits decoding issues.

## Identity

- [`parakeet-rs`](https://crates.io/crates/parakeet-rs) v0.3.7 (2026-07-28), MIT OR Apache-2.0 · https://github.com/altunenes/parakeet-rs · docs.rs/parakeet-rs
- Pure Rust, no sidecar/server. Created 2025-10-15; 28 releases in ~10 months, none yanked. 385 stars, 3 open issues, accelerating adoption (~45k recent downloads).
- Effectively single-maintainer (altunenes) though PR intake is healthy.

## API shape (all loaders follow `X::from_pretrained(dir, Option<ExecutionConfig>)`)

| Struct | Mode | Core calls | Weights |
|---|---|---|---|
| `Parakeet` (CTC) | Offline EN, punct+caps | `transcribe_samples(audio, 16000, 1, Some(TimestampMode::Words))` → `.text`, `.tokens[]` | `onnx-community/parakeet-ctc-0.6b-ONNX` |
| `ParakeetTDT` | Offline multilingual (25 langs) | same signature | `istupakov/parakeet-tdt-0.6b-v3-onnx` (v3, not v2) |
| `ParakeetEOU` | Real-time streaming + end-of-utterance | `transcribe(&chunk, bool)?` | `nvidia/parakeet_realtime_eou_120m-v1` (160 ms chunks, `CHUNK_SIZE=2560`) |
| `Nemotron` (+ `NemotronMode::Multilingual`) | Cache-aware streaming, punct | `transcribe_chunk(&chunk)?`, `set_target_lang("es-ES")` | `nemotron-speech-streaming-en-0.6b` (int8/int4 mirrors exist); example uses 560 ms chunks — 80/160/1120 ms tiers are separate swappable model files |
| `ParakeetUnified` | Offline + buffered-streaming RNNT (PR #91) | — | `bobNight/parakeet-unified-en-0.6b-onnx` |

Both plan targets supported natively: `ParakeetUnified` AND `Nemotron`.

Feature-gated extras: `MultitalkerASR`, `CohereASR`, `sortformer` diarization.

## Backend & Windows

- `ort ^2.0.0-rc.13` → ONNX Runtime 1.28. Prebuilt binaries auto-fetched (`download-binaries`). CUDA EP requires CUDA 13; **DirectML is the native Windows GPU path**, CPU auto-fallback if GPU init fails. No Windows compile test performed (research-only).

## Decode-correctness track record

Fixed quickly, mostly via community PRs: number-word concatenation (#52), duration-step applied to wrong token types in TDT (#69), continuous mel buffer preventing decoder stalls (#59), dropped single-word utterances in EOU streaming (#115). macOS CoreML flagged unstable by README itself.

## Other wrappers found (2025–2026)

- [`nemotron-asr`](https://lib.rs/crates/nemotron-asr) v0.1.0 (single unstable release) — bindings to GGML-based `m1el/nemotron-asr.cpp`
- [`sherpa-onnx`](https://crates.io/crates/sherpa-onnx) 1.13.5 — primary choice (see sherpa-onnx-streaming.md)
- [`transcribe-rs`](https://crates.io/crates/transcribe-rs) — multi-engine abstraction (backend list unverified)
- [`hyprwhspr-rs`](https://crates.io/crates/hyprwhspr-rs) — Linux dictation app built on parakeet-rs (proof of dictation-grade usage)

## OpenWhispr precedent

Release [1.7.6] – 2026-07-18: "NVIDIA Nemotron streaming transcription models … true streaming models" via bundled sherpa-onnx runtime upgraded to 1.13.4 ("which Nemotron requires for accurate decoding") (#1131); streaming dictation commits in a single pass, tail flush, ~halved per-dictation CPU (#1238). Their changelog also documents the cost of the sherpa sidecar approach (firewall prompts #1090, tar drive-letter failures #284, IPv6 startup races) — friction a direct `parakeet-rs` dependency avoids.
