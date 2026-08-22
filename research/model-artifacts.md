# Model artifacts: sizes, metadata, feature config

Researched 2026-08-22. Sizes from HF API file trees (exact LFS bytes) + HTTP HEAD on release assets; config values read **directly from shipped ONNX binaries** (embedded protobuf metadata), not inferred from scripts.

## Download sizes (core payload = encoder+decoder+joiner+tokens)

| Artifact | Core payload | Compressed (.tar.bz2) |
|---|---:|---:|
| [csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms](https://huggingface.co/csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms) | **663,048,739 B (632.3 MiB)** | 501,358,456 B (~501 MB) via [release asset](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms.tar.bz2) |
| [csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25](https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25) | **661,919,414 B (631.2 MiB)** | 463,945,379 B (~464 MB) via release asset |
| [bobNight/parakeet-unified-en-0.6b-onnx](https://huggingface.co/bobNight/parakeet-unified-en-0.6b-onnx) int8 set | 663,093,317 B (632.4 MiB) | n/a (HF repo) |

- Both plan models are essentially identical uncompressed (~632 MiB); compressed downloads differ (464 vs 501 MB).
- **bobNight layout differs** (fused `decoder_joint.int8.onnx` + external `.onnx.data` + sentencepiece `tokenizer.model`, exported for onnx-asr not sherpa-onnx) → a parakeet-rs fallback backend **cannot share downloads** with the sherpa path; it would re-download its own artifact set.
- Cross-checks: OpenWhispr docs list parakeet-unified ~631 MB / nemotron ~632 MB ([docs.openwhispr.com/guides/local-models](https://docs.openwhispr.com/guides/local-models)).
- **Resident RAM after load: NOT PUBLISHED ANYWHERE found** (no README requirements, no RSS numbers in issues/docs/blogs). Must measure locally after model load and feed real numbers into `ModelSpec.min_ram_gb`.

## FeatureConfig values (from embedded encoder metadata)

| Key | Unified streaming 240ms | Nemotron streaming 80ms |
|---|---|---|
| sample_rate | 16000 | 16000 |
| feat_dim | **128** | **128** |
| mel type | librosa-style mel spectrogram (low_freq=0, high_freq=8000, is_librosa=true, hann, remove_dc_offset=false, dither=0 — forced by runtime PostInit per PR #3575) | same class (preprocessor: 16000 Hz, 128 feats, n_fft 512, window 0.025 / stride 0.01 hann, normalize 'NA') |
| normalize_type | **per_feature** | **none (empty string)** |
| subsampling_factor | 8 | 8 |
| vocab_size | 1024 (+blank → 1025 at runtime) | 1024 |
| pred_rnn_layers / pred_hidden | 2 / 640 | 2 / 640 |
| model_type tag | `nemo_parakeet_unified_streaming` (buffered_streaming=1) | EncDecHybridRNNTCTCBPEModel, transducer branch only |
| buffer keys | `{left,chunk,right}_{encoder,feature}_frames` (NO `buffer_` prefix): left_encoder=70, chunk_encoder=1, right_encoder=2 → latency=(1+2)×80ms=240ms; left_feature=560, chunk_feature=8, right_feature=16 | chunk_shift=8 frames (80 ms), window_size=17, cache dims channel(24,70,1024)/time(24,1024,8) |

Export-script latency presets for unified (encoder frames L/C/R): 1120ms=(70/7/7), 560ms=(70/2/5), 240ms=(70/1/2).

## Critical wiring facts (silent-degradation traps)

1. **`feature_dim` must be set to 128** — the Rust `sys::FeatureConfig` default is `{sample_rate: 16000, feature_dim: 80}`; wrong dim silently degrades quality.
2. **Normalization differs between the two models** (unified=`per_feature`, nemotron=none). Do not hardcode one value for both; verify at wiring time whether the runtime reads `normalize_type` from encoder metadata or needs it set.
3. Earlier research note had wrong key names (`buffered_nemo_rnnt` / `buffer_*`) — actuals above; C++ runtime hard-exits if streaming keys don't match.

Sources: export scripts [scripts/nemo/parakeet-unified-en-0.6b/export_onnx_streaming.py](https://github.com/k2-fsa/sherpa-onnx/blob/master/scripts/nemo/parakeet-unified-en-0.6b/export_onnx_streaming.py) + [notes.md](https://github.com/k2-fsa/sherpa-onnx/blob/master/scripts/nemo/parakeet-unified-en-0.6b/notes.md), [scripts/nemo/nemotron-speech-streaming-en-0.6b/export_onnx.py](https://github.com/k2-fsa/sherpa-onnx/blob/master/scripts/nemo/nemotron-speech-streaming-en-0.6b/export_onnx.py), k2-fsa docs nemotron-streaming.rst.
