# sherpa_onnx streaming lifecycle + issue-tracker maturity

Researched 2026-08-22 (web + source-level). Closes plan gaps: recognizer reset semantics, F9 session mapping, issue-tracker pass for Nemotron/parakeet-unified.

## Object model (Rust 1.13.x, real signatures)

From docs.rs/sherpa-onnx 1.13.5 source ([online_asr.rs](https://docs.rs/sherpa-onnx/latest/src/sherpa_onnx/online_asr.rs.html)) — thin RAII over C API; `OnlineRecognizer` and `OnlineStream` are both `unsafe impl Send + Sync`:

```rust
impl OnlineRecognizer {
    pub fn create(config: &OnlineRecognizerConfig) -> Option<Self>; // None = native init failed, NO error string
    pub fn create_stream(&self) -> OnlineStream;
    pub fn decode(&self, stream: &OnlineStream);
    pub fn is_ready(&self, stream: &OnlineStream) -> bool;
    pub fn is_endpoint(&self, stream: &OnlineStream) -> bool;
    pub fn reset(&self, stream: &OnlineStream);
    pub fn get_result(&self, stream: &OnlineStream) -> Option<RecognizerResult>;
}
impl OnlineStream {
    pub fn accept_waveform(&self, sample_rate: i32, samples: &[f32]);
    pub fn input_finished(&self);
}
pub struct RecognizerResult { pub text: String, pub tokens: Vec<String>, pub timestamps: Option<Vec<f32>>, pub segment: Option<i32>, pub start_time: Option<f32>, pub is_final: bool }
```

Canonical loop: `accept_waveform` → `while is_ready { decode }` → optional endpoint check → at end: `input_finished()` → drain `is_ready/decode` → `get_result().text`.

### Rust-vs-C++ traps

- **Endpoint rule defaults are `0.0/0.0/0.0`** (C++ defaults are 2.4/1.2/20). Leaving them zero makes rules misfire — always set explicitly when `enable_endpoint=true`.
- `config.reset_encoder` (C++) NOT exposed through Rust/C-API struct.
- `EndpointRule.must_contain_nonsilence` not settable from Rust.
- Naming: Rust is `decode`/`decode_multiple_streams`/`get_result` (not Python's `decode_stream`/etc.). No `stream.result()`.

## What state persists, what reset clears

Per-stream state across decodes (from `online-stream.{h,cc}`): feature-extractor buffer (**NOT cleared by Reset**), hypothesis tokens/`num_trailing_blanks`, segment counter, encoder cache states, NeMo predictor LSTM states + last decoder output (#3575 machinery), plus misc caches.

`reset(stream)` on the parakeet-unified impl bumps segment, clears result, re-inits predictor LSTM states, rolls frame counters — but keeps features. Generic cache-aware impl additionally keeps trailing `context_size` tokens as ys-context unless `reset_encoder` (C++-only).

**A brand-new `create_stream()` is a fully clean slate and cheap** (no model reload).

## Session mapping (decided)

- **Record mode**: fresh stream per F9 press. Pump during hold; on release push **~0.6 s of zeros** tail padding → `input_finished()` → drain → take `get_result().text` → drop stream. Fresh-per-session sidesteps every stale-state trap (features kept on reset, ys-context retention, LSTM residue).
- **Live mode**: long-lived stream during hold, partials from `get_result().text`; on release same tail-pad → `input_finished()` → drain → commit. Prefer NO mid-hold resets for normal dictations (segment bookkeeping across resets is the top cause of duplicated/dropped text). Only for very long holds: enable endpointing with explicit rules (e.g. rule3 ≈ 20 s), snapshot committed segment → `reset` → continue.
- Precedent: OpenWhispr uses fresh connection (= fresh server-side stream) per dictation, `ONLINE_END_TAIL_PADDING_S = 0.6`, no mid-session resets found. Their constant comment: "Must cover the model's 560ms chunk so the flush decodes the final words."

## Issue-tracker pass (Apr–Aug 2026)

Version anchors: v1.13.2 May 13 = first with unified streaming (#3575) · **v1.13.4 Jul 7** (ORT bump; Windows asset suffix; OpenWhispr's pin) · **v1.13.5 Aug 11** = both accuracy fixes · v1.13.6 Aug 18 current.

| Issue | What | Severity / status |
|---|---|---|
| [#3575](https://github.com/k2-fsa/sherpa-onnx/pull/3575) | buffered RNNT streaming path for Parakeet Unified | merged 2026-05-09; E2E-tested vs OpenWhispr; review flagged predictor-state double-consumption risk |
| [#3782](https://github.com/k2-fsa/sherpa-onnx/issues/3782) → [#3785](https://github.com/k2-fsa/sherpa-onnx/pull/3785) | Nemotron **int8** WER regression (cross-chunk corruption of prediction-LSTM state; Fleurs 6.20→15.04%) | fixed in v1.13.5. Residual **acknowledged +1–3% WER vs fp32 NeMo from int8 quantization** — unresolved, inherent |
| [#3853](https://github.com/k2-fsa/sherpa-onnx/issues/3853) → [#3857](https://github.com/k2-fsa/sherpa-onnx/pull/3857) | empty stream poisons batched streams (float32 catastrophic cancellation in `NemoNormalizePerFeature`) | fixed v1.13.5; irrelevant if single-stream |
| [#3572](https://github.com/k2-fsa/sherpa-onnx/issues/3572) | `modified_beam_search`/hotwords unsupported for Nemotron/unified streaming | OPEN — greedy_search only |
| [#3767](https://github.com/k2-fsa/sherpa-onnx/issues/3767) | offline NeMo TDT decodes EMPTY on Windows | OPEN — different model family than ours, but watch |
| [#3791](https://github.com/k2-fsa/sherpa-onnx/issues/3791) | ORT 1.27 SME conv silently corrupts some models (macOS M4); nemotron listed NOT affected | workaround shipped (ORT 1.27.1 in v1.13.5); cautionary: numerics regressions ride ORT bumps |

No memory-leak/crash reports specific to our two streaming paths found in window searched.

## Verdict + discipline

Conditionally mature: bet on it, **pin sherpa-onnx ≥ 1.13.5 (ideally 1.13.6)** — both accuracy fixes exist only in v1.13.5+, so OpenWhispr's 1.13.4 pin actually predates them. Discipline: fresh stream per F9 press, greedy_search, explicit nonzero endpoint rules, 0.6 s tail before `input_finished()`, smoke-test our exact int8 pair against pinned version before locking.
