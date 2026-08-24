# Plan: parakeet-ptt → Rust GPUI dictation app

Port of `C:\Users\Yashraj\Downloads\parakeet-ptt` (Bun + Python) to a single Rust binary.
Research backing this plan: [research/sherpa-onnx-streaming.md](../research/sherpa-onnx-streaming.md), [research/parakeet-rs-fallback.md](../research/parakeet-rs-fallback.md).

## Unchanged from the original app

Audio capture (`cpal`), hotkeys (`global-hotkey`), paste (`arboard` + `enigo`), HUD (`GPUI`), model fetching (`hf-hub`), phase state machine (loading → idle → recording → transcribing), pending-start queue during load, `MIN_AUDIO_SECS` gate, filler-word cleanup, `asr_s`/`load_s` timing events, toggle-F9 semantics.

## ASR layer

### Model selection

Single model, both modes: `nvidia/nemotron-speech-streaming-en-0.6b` @80 ms — genuinely stateful streaming (cache-aware FastConformer-RNNT, not buffer-replay). The original two-model split (record=parakeet-unified@240ms for accuracy / live=nemotron@80ms for latency) is preserved in git history; it was undone by the unified removal in the [decision audit](#decision-audit--drift-from-original-decisions-2026-08-23). The `ModelSpec` registry keeps one entry today and is the swap point for any future model (e.g. phase 11's offline-batch TDT candidate).

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

Mode-to-model is a lookup, not a branch — today a single registry entry serves both modes; adding or swapping a model later = new registry entry + config id, not new inference code.

### Session lifecycle (F9 mapping — decided)

From [research/sherpa-lifecycle-and-issues.md](../research/sherpa-lifecycle-and-issues.md):

- **Fresh `create_stream()` per F9 press in both modes.** Stream creation is cheap (no model reload) and a fully clean slate — sidesteps every `reset()` trap (feature buffer kept, zipformer ys-context retained, LSTM residue).
- **Record**: pump `accept_waveform`/`is_ready`/`decode` during hold → on release push ~0.6 s zero tail padding (covers right-context chunk; OpenWhispr's proven constant) → `input_finished()` → drain → take text → drop stream.
- **Live**: long-lived stream during hold, partials from `get_result().text`; on release identical tail-pad → drain → commit. No mid-hold resets for normal dictations (segment bookkeeping across resets is the top cause of duplicated/dropped text); explicit endpointing only for very long holds (rule3 ≈ 20 s) — and set the rules explicitly, Rust config defaults are 0/0/0 unlike C++'s 2.4/1.2/20.
- `decoding_method = greedy_search` — beam search + hotwords unsupported on this path (#3572 open).

### Memory residency across mode switches (**LOCKED** after phase 3+4 measurements)

Load/unload per mode switch: only the active mode's model resident at a time. Rationale: the app lives in the background permanently, mode flips are rare, reload after first fetch is fast via OS page cache, and the pending-start queue already absorbs load latency. Consequence: the onboarding RAM check validates against **one** model's footprint plus headroom, not the sum of both.

Measured (release build, x64-emulated dev box — absolute times shrink on native hardware, ratios hold):

| Model | Cold load | Unload | Warm reload | Resident RSS |
|---|---|---|---|---|
| nemotron@80ms | 11.9–13.7 s | 0.12 s | 7.0 s | 755 MiB |
| unified@240ms | 12.8 s | 0.19 s | 12.5 s | 721 MiB |

Verdict: unload is free (~0.15 s), reloads land behind the pending-start queue, and one-resident keeps steady-state at ~750 MiB instead of ~1.4 GiB. Design locked; revisit only if native-build timings change the picture.

## Decision audit — drift from original decisions (2026-08-23)

| Original decision | Current state | Why |
|---|---|---|
| two-model split: record=unified@240ms, live=nemotron@80ms | **unified removed from the app entirely** (2026-08-24); nemotron@80ms is the single model for both modes; registry/spec architecture kept so another model can slot in later | unified hard-collapses to *empty output* on real mic audio (reproduced; degrades below a level/SNR cliff where nemotron merely degrades) and decodes ~6× slower even when it works — a 2.6 s dictation took ~21 s and returned zero characters |
| record = unified@240ms | record runs nemotron@80ms; unified stays in the registry (selectable, cached) and is the phase-11 offline-batch candidate | native decode RTF: unified ~15× vs nemotron ~2.5× — unified's post-stop waits broke real use ([PHASE11-NOTES.md](PHASE11-NOTES.md)) |
| paste restores previous clipboard (legacy `worker.py` parity) | transcript deliberately **stays on the clipboard**, no restore | user preference: re-paste manually anywhere afterwards |
| no focus management needed (legacy pynput global hooks never steal focus) | paste follows the **live** foreground window at commit time; only when our HUD holds focus do we rescue it (capture at record start → `AttachThreadInput` + `SetForegroundWindow`) | GPUI window owns focus at launch; synthesized keys would otherwise land on the HUD. First attempt used an ALT-tap trick — injected Alt latched target menu bars; AttachThreadInput replaced it |
| x86_64 forced target (emulation) | native aarch64 via upstream CI static libs | phase 10 |
| ModelSpec registry + mode lookup | shipped with phase-7 onboarding (`REGISTRY`, per-mode config ids); before that it was a hardcoded enum | two models didn't justify a registry until the alternatives UI needed one |
| explicit endpoint rules for very long live holds | `enable_endpoint = false` everywhere | 300 s hold test BOUNDED (RSS flat, partials advancing); revisit for >10 min holds |
| `asr_s`/`load_s` timing events | structured pipeline logging to stderr + `dictation.log` (RTF, RMS/peak, paste trace, focus decisions) | richer and file-persisted |

Unmoved from the original design: `AsrBackend` trait behind a worker thread that owns all sherpa calls · fresh `create_stream()` per F9 · greedy_search · one-model residency LOCKED · pending-start queue through load · `MIN_AUDIO_SECS` gate · filler cleanup quirk-pinned to `worker.py` tests · cpal mono downmix + resample.

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
3. ✅ **Done** — ASR live mode: Nemotron @80ms via sherpa-onnx `=1.13.5` ([src/asr/](../src/asr/) — `AsrBackend` trait + `NemotronBackend` + worker thread owning all sherpa calls; raw f32 chunks from cpal w/ 16k-first/fallback-resample). Checklist outcomes: ☑ normalize_type confirmed **read from encoder metadata** — empirical proof: smoke test decodes repo's `test_wavs/0.wav` to an exact match vs `trans.txt` with no normalize field in Rust config · ☑ nemotron metrics logged: **cold load 11.9 s @ 755 MiB RSS · unload 0.12 s · warm reload 7.0 s** (F10 debug cycle); unified-model numbers land in phase 4, then the residency design locks.
4. ✅ **Done** — ASR record mode: unified @240ms (`UnifiedBackend` behind the same trait; worker now swaps models on mode switch, chip click → Loading → Ready; partials rendered only in Live). Checklist outcomes: ☑ `per_feature` honored from encoder metadata (smoke transcript **character-exact** vs repo reference) · ☑ unified metrics: cold 12.8 s @ 721 MiB · unload 0.19 s · warm reload 12.5 s → **residency design locked** (see table above) · ☑ 5-min hold test: RSS growth ≈ **0 MiB** over a 300 s session, partials advancing through second 298/300, finalize clean 6.3 s → **BOUNDED**, no endpoint-flush safeguard needed (ran on nemotron; both backends share identical stream lifecycle, and unified's per-step state is the same windowed-buffer mechanism).
5. ✅ **Done** — Paste path ([src/paste.rs](../src/paste.rs)): legacy-faithful cleanup (filler `\b{src}\b` strip → filler regex fixpoint loop for chained fillers → whitespace/punctuation fixes; quirk-pinned by tests to match `worker.py` exactly), `MIN_AUDIO_SECS` gate via worker-reported `Committed{text, duration_secs}`, arboard set + enigo Ctrl+V on a per-commit std::thread, HUD confirmations, honest "transcribing… Ns" ticker. Clipboard behavior deliberately diverged from legacy (transcript stays on clipboard); focus handling per the audit above.
6. **Folded into phase 11** — boundary redecode as originally scoped assumed the legacy buffer-replay design and its longest-suffix merge; our streaming architecture has neither, and a genuine full-context correction pass requires an offline decoder we don't ship. Phase 11 option 1 (offline-batch record decode) delivers the same accuracy win at its root cause.
7. ✅ **Done** — Onboarding & model switching: `ModelSpec` registry ([src/asr/model.rs](../src/asr/model.rs)), `%APPDATA%/raycast-dictation/config.json` persistence ([src/config.rs](../src/config.rs), skip-onboarding once present, unknown ids fall back with warning), single-window content swap to an onboarding screen on first launch: RAM/core detection via sysinfo with min-RAM warning (never blocks), model card with cached-state chip, background download streaming progress into the view over mpsc; F9 during setup queues through the existing pending-start flag and auto-starts once the model lands; worker takes `ModelSelection {model}`. Settings screen doubles as a **model manager**: Reveal in Explorer (cache dir), Delete model (with OS-error surfacing for locked files), Download again (delete + re-fetch), Run setup again (clears config, replays first-run onboarding), and startup routing that opens recovery onboarding when config exists but the cache is missing instead of silently re-downloading.
6½. **Unified removal** (2026-08-24): `src/asr/unified.rs` deleted, `ModelKind::Unified` gone, config schema collapsed to `{model}` (legacy two-slot configs fail serde → setup reruns), asr_smoke lost its unified branch but kept all diagnostic env knobs (`ASR_SMOKE_FEED_SAMPLES/GAIN/NN_RESAMPLE/PAD_LEAD_SECS/TRUNCATE_SECS/RAW`) and gained an `ASR_DUMP_AUDIO` session dump in the worker for replaying real mic audio (`ASR_SMOKE_RAW`) — the tooling that diagnosed unified's empty-output collapse.
8. Polish: transparent capsule window.
9. Tuning: latency benchmarks vs old Python baseline, WER spot-check on own voice samples — including the acknowledged +1–3% int8 residual penalty (#3782).
10. ✅ **Done** — Native ARM64 sherpa-onnx: upstream CI already publishes `sherpa-onnx-v1.13.5-win-arm64-static-MT-Release-lib.tar.bz2` (all 14 libs match build.rs inventory; ORT 1.27.1 static) → staged in `.native-libs/aarch64-windows/` (gitignored), wired via `SHERPA_ONNX_LIB_DIR` in `.cargo/config.toml [env]`; target flipped to `aarch64-pc-windows-msvc`; aws-lc-sys armv8 asm needed standalone clang-cl (`CC/CXX` env). Emulation penalty gone. **Thread-count sweep** (decode RTF, native): nemotron 1t=5.1× / **2t=2.5×** / 3t=2.8× / 4t=3.0× / cores=8.8× — ORT intra-op sweet spot at 2, now the default (`ASR_THREADS` env overrides). Unified: 25.9×→**15.2×** with 2 threads + native-chunk feeds. Transcripts still MATCH both models.
11. **Reaching real-time on this hardware** (parked — see [PHASE11-NOTES.md](PHASE11-NOTES.md)): nemotron@2t decodes ~2.5× audio duration, unified ~15×; leading option is offline-batch decode for Record mode (what the legacy app effectively did), then DirectML EP.

Observability: everything logs through [src/logging.rs](../src/logging.rs) (`log!` macro) → stderr **and** `dictation.log` in the project dir (gitignored): audio device/fallback info, model load/unload/switch timings + resident MiB, per-session decode wall time + RTF + finalize time, raw transcript vs cleaned output with gate decisions, and every paste step (clipboard set, focus decision with target hwnd+title, key send).

## Risks & known constraints

- int8 Nemotron carries an **acknowledged residual +1–3% WER vs fp32 NeMo** (cross-chunk LSTM corruption was fixed in v1.13.5, but quantization penalty remains) — inherent, benchmark in phase 9.
- Rust crate gaps: ~25% documented; `OnlineRecognizer::create` returns `Option` with no error string; `reset_encoder` and `EndpointRule.must_contain_nonsilence` not exposed; endpoint-rule defaults are 0/0/0 and must be set explicitly.
- Silent-numerics regressions ride bundled ONNX Runtime bumps (#3791) → pin exact sherpa-onnx versions; smoke-test our exact model pair against the pinned binaries before locking.
- Open Windows bugs in the broader NeMo family (offline TDT empty decode #3767, non-ASCII path misdecode #3885) — different subsystems, watch only.
- GPUI transparency / always-on-top fields don't appear in any example at our pinned rev — expect to dig into `WindowOptions` source directly in phase 8.
- ~~x64 emulation~~ resolved (phase 10): native aarch64 build via upstream CI libs. Remaining constraint: CPU-only static ORT + 0.6B transducer = decode RTF ~2.5× (nemotron) / ~15× (unified) on this SoC even natively — see phase 11 for the path to real-time.
- ~~Unified@240ms decodes ~5× slower than nemotron@80ms on identical audio/config~~ resolved by removal (see decision audit) — nemotron@80ms is the only model; the phase-11 offline-batch path would introduce a *different* (non-streaming TDT) model if pursued.
