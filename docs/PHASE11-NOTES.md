# Phase 11 — reaching real-time on this hardware (backlog note)

Written 2026-08-23 so we don't lose context. Parked deliberately: first make
phases 4/5/10 solid end-to-end (paste + logging verification), then return here.

## The problem

Native ARM64 sherpa-onnx (phase 10) removed the x64-emulation penalty, but a
0.6B streaming transducer on this SoC's CPU is still not real-time. Measured
decode RTF (wall time / audio duration), release build:

| Model | 1 thread | 2 threads (current default) | notes |
|---|---|---|---|
| Nemotron (both modes) | ~5.1x | **~2.5x** | ORT intra-op sweet spot at 2; more threads regress (sync overhead on small per-chunk matmuls). ~~Canary unified (Record)~~ removed from the app 2026-08-24 (empty-output collapse on real mic audio + ~15x RTF); its row is history. |

Consequences: Live partials lag behind speech (~1.5s behind per second spoken);
Record mode waits ~15x the recording length after stop before pasting.
`ASR_THREADS` env var overrides the default if future hardware differs.

## Options, in rough order of expected payoff

1. **Offline-batch decode for Record mode.** The legacy Python app used
   parakeet-tdt-0.6b-v2 (non-streaming TDT, int8) via full-buffer batch decode
   and *felt fine to the user* - batch decode processes the whole utterance in
   one pass with far less per-chunk overhead than windowed streaming. Record
   mode does not need streaming semantics; it needs fast final text after stop.
   Candidate: `istupakov/parakeet-tdt-0.6b-v2-onnx` (int8) through
   `onnx-asr`-equivalent Rust inference or sherpa's OfflineRecognizer with a
   TDT model. This likely gets Record under 1.0x RTF without GPU work.
2. **DirectML execution provider.** Static-MT ORT bundles are CPU-only EPs.
   Swapping to dynamically-linked ORT (or building sherpa against dynamic ORT)
   unlocks `provider=directml` and any iGPU/dGPU on the box. Biggest ceiling,
   most build work (revisits the phase-10 shortcut).
3. **Smaller / quantized Live model.** Nemotron has no smaller sibling; option
   would be int8-quantizing the encoder or picking another small streaming EOU
   transducer for Live mode only.
4. **Thread tuning revisit on real hardware changes** - keep the sweep
   methodology from phase 10 (1/2/3/4/cores) when testing options 1-2.

## Decision rule

Ship option 1 if it lands Record < ~2x RTF (wait after stop stays tolerable);
pursue option 2 only if Live mode matters enough to justify the ORT linking
work, since Live usability needs < 1.0x plus partial latency headroom.
