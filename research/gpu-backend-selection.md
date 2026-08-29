# GPU and NPU backend selection for Amanuensis

Researched 2026-08-29. Decision-grade notes for choosing compute backends per platform, with automatic selection, so every user gets the fastest path on their actual hardware. Two workloads matter: the sherpa-onnx ASR model (Nemotron streaming now, Moonshine record mode later) and a future GGUF normalizer run through llama.cpp.

## What this will do, in plain words

At onboarding the app figures out what hardware it is on: NVIDIA, AMD, or ARM CPU. It takes the candidate list for that hardware and times each one on a short clip. The winner is stored, and the app uses it from then on. If the device changes later, the test runs again.

Where the GPU can and cannot be used:

- Speech recognition: CUDA on NVIDIA, CPU everywhere else. No shipped build of the engine has Vulkan, DirectML, or AMD-GPU support, so on AMD machines speech always stays on CPU.
- Text polish, the rewrite step we may add later: uses llama.cpp, which can run on CUDA, Vulkan, or OpenCL. This is the step where CPU-versus-GPU is actually tested.

On this Snapdragon (ARM CPU) the test has effectively already run: speech stays on CPU because no GPU build exists for it, and the polish step's Adreno GPU measures slower than the CPU, so CPU wins there too. The app already runs close to realtime on this machine.

## Baseline facts

| workload | measured |
|---|---|
| Nemotron ASR cpu, 2 threads | RTF 0.96-1.01x, live decode |
| Nemotron ASR cpu, 4 threads | RTF 0.84-0.87x |
| Nemotron ASR cpu, 1 thread | RTF 1.15-1.19x (drops audio) |
| s1-mini normalizer cpu, 8 threads | pp 370 t/s, tg 45.1 t/s, clips 3.8-5.5 s |
| s1-mini normalizer Adreno OpenCL gpu | pp 482 t/s, tg 30.5 t/s, clips 2.5-3.4x slower end-to-end |
| GPU probes (dml/vulkan/cuda/coreml/openvino) | all fall back to cpu on the prebuilt win-arm64 sherpa lib |

## How sherpa-onnx picks a provider

`sherpa-onnx/csrc/provider.cc` accepts exactly these strings, lowercased: `cpu`, `cuda`, `coreml`, `xnnpack`, `nnapi`, `trt`, `directml`, `spacemit`. Anything else logs "Unsupported string: ... Fallback to cpu" and runs cpu.

That corrects our earlier probe log. `dml`, `vulkan`, and `openvino` are not sherpa strings in any build, so those probes never had a chance. `cuda` and `coreml` are real strings, but on the prebuilt win-arm64 lib they fail later at session creation with "Please compile with -DSHERPA_ONNX_ENABLE_GPU=ON. Available providers: %s. Fallback to cpu!", because the lib ships CPU-only.

Two failure modes, two log signatures:

1. Wrong string. "Unsupported string: X. Fallback to cpu". The string never maps to an EP.
2. Right string, EP not compiled in. "Please compile with -DSHERPA_ONNX_ENABLE_GPU=ON". The lib was built without that EP.

Both are silent from the API side. `OnlineRecognizer::create` returns `Some` and decodes on cpu. The app gets no signal it lost the GPU.

Provider strings accept options after a comma. `session.cc` splits on the first comma and parses known keys, so "directml,EnableCpuMemArena=0" and "cpu,GraphOptimizationLevel=99" work. That is the channel for per-EP tuning from the current Rust crate.

Transducer models get a special rule: `trt` runs the encoder on TensorRT and the decoder/joiner on CUDA. For a 0.6B model `trt` is pointless; use `cuda` directly.

The Rust wiring today sets nothing. `src/asr/nemotron.rs` leaves `provider` unset (cpu default) and takes thread count from `threads()` in `src/asr/mod.rs`, which reads `ASR_THREADS`, default 2. Adding a provider means setting `config.model_config.provider`, which the probe already does in `src/bin/asr_gpu_probe.rs`.

## Prebuilt sherpa-onnx libs per platform

From the v1.13.5 release assets (matching our pinned `sherpa-onnx = "=1.13.5"`):

| platform | prebuilt GPU lib | notes |
|---|---|---|
| win-x64 NVIDIA | `sherpa-onnx-v1.13.5-cuda-12.x-cudnn-9.x-onnxruntime1.27.1-win-x64-cuda.tar.bz2` and a `cuda-13.x` twin | point `SHERPA_ONNX_LIB_DIR` at one of these, provider `cuda`. No source build needed |
| win-x64 AMD | none with DML | DirectML EP not compiled in any Windows prebuilt. DML needs a from-source ORT build, or the WinML runtime |
| win-arm64 | none, CPU only | our measured reality. No DML, no vulkan, no cuda in any arm64 artifact |
| linux-x64 NVIDIA | `sherpa-onnx-v1.13.5-cuda-12.x/13.x-cudnn-9.x-onnxruntime1.27.1-linux-x64-gpu.tar.bz2` | same story as win-x64 |
| linux-x64 AMD | none with ROCm | cpu default today |
| linux-aarch64 (Jetson) | `sherpa-onnx-v1.13.5-linux-aarch64-shared-gpu-onnxruntime-1.11.0/1.16.0/1.18.0-lib.tar.bz2` | CUDA/TRT for Jetson-class boards |
| osx-arm64, osx-x64 | CPU libs; `coreml` works only if the shipped lib compiled it in | verify with the log probe, see detection below |

The win-x64 CUDA libs are the one place a user gets GPU ASR without compiling onnxruntime. Everywhere else on desktop, sherpa GPU means building from source.

## onnxruntime execution providers, what is real where

Native onnxruntime EPs: CUDA, TensorRT, DirectML, CoreML, OpenVINO, ROCm, MIGraphX, QNN, NNAPI, WebGPU, XNNPACK, ArmNN, ACL. None of them exist unless compiled into the lib or registered as a plugin.

Platform-relevant facts:

- DirectML ships in-box only through WinML (`windows.ai.machinelearning.dll`), which includes CPU plus DirectML. The DirectML EP in plain onnxruntime builds x64 and x86 only, so there is no arm64 DirectML NuGet. DirectML is in sustained engineering at Microsoft.
- onnxruntime has no Windows Vulkan EP. The cross-vendor native GPU EP is WebGPU (Dawn), which drives D3D12 on Windows. The native WebGPU EP is a plugin library, `onnxruntime_providers_webgpu.{dll,so,dylib}`, registered at runtime by path. No prebuilt until you build it.
- An auto EP exists since onnxruntime 1.22. `SessionOptions.SetEpSelectionPolicy` takes policies (MAX_PERFORMANCE, MAX_EFFICIENCY, PREFER_NPU, PREFER_GPU, PREFER_CPU, MIN_OVERALL_POWER) and picks the best EP and device on Windows. sherpa cannot reach it through its provider string, and WinML drives the common path anyway.
- Windows ML (24H2+) downloads vendor EPs on demand: QNN for Snapdragon, OpenVINO for Intel, NvTensorRtRtx and MIGraphX and VitisAI for discrete GPUs. `GetEpDevices()` enumerates what is registered. This is a different runtime from the statically-linked onnxruntime inside the sherpa crate.

## llama.cpp backends and prebuilt coverage

Prebuilt assets from the b10675 build (the one we shipped and benched):

| platform | builds available |
|---|---|
| win-x64 | cpu, cuda-12.4, cuda-13.3, openvino, rocm-7.14, sycl, vulkan |
| win-arm64 | `win-cpu-arm64` (plain cpu), `win-opencl-adreno-arm64` (OpenCL tuned for Adreno), `win-cuda-13.4-arm64` (needs an NVIDIA dGPU, not for Snapdragon) |
| linux-x64 | cpu, vulkan, rocm-7.14, sycl (fp16/fp32), openvino. No CUDA prebuilt on Linux, license reason |
| linux-arm64 | cpu, vulkan-arm64 |
| macos | arm64 (Metal inside), x64 |

No win-vulkan-arm64 exists. On this Snapdragon the only GPU backend that ships is OpenCL-Adreno.

How llama.cpp picks a device at runtime:

- Backend registry orders registration, then `ggml_backend_init_best` picks the first device of type GPU, else IGPU, else CPU. That reads "first discrete GPU found, otherwise integrated, otherwise CPU".
- The Vulkan backend hides iGPUs by default and only enumerates them when no dGPU exists. `GGML_VK_VISIBLE_DEVICES=0,1` overrides the list. `GGML_DISABLE_VULKAN` turns the whole backend off.
- OpenCL selects a device via `GGML_OPENCL_DEVICE` (index or name substring). CUDA and HIP use `CUDA_VISIBLE_DEVICES` and `HIP_VISIBLE_DEVICES`.
- The CLI exposes `--list-devices` and `--device <dev1,dev2>`. Backend DLLs can load dynamically with `GGML_BACKEND_DL`, which is how the prebuilt zips ship `ggml-vulkan.dll` next to the exe.

whisper.cpp shows the failure mode of naive auto-detect: `whisper_backend_init_gpu` walks registered backends and takes the first GPU, so CUDA always beats Vulkan even when Vulkan measures faster on the same box. Auto-detection is a heuristic, not a choice. For Amanuensis the order must come from measurement, with the vendor order only a first guess.

## Per-platform matrix, recommended order

ASR column is the sherpa provider string plus the lib it needs. Normalizer column is the llama.cpp asset and the device order to try. Bench decides, these are just the seeds.

| case | ASR order to try | normalizer order to try | notes |
|---|---|---|---|
| Windows x64 + NVIDIA | `cuda` (CUDA prebuilt lib), then `cpu` | `win-cuda-13.3` (or 12.4 for old drivers), then `cpu` | the one easy GPU win. Skip `trt` |
| Windows x64 + AMD | `cpu` (no DML prebuilt) | `win-vulkan-x64`, `win-rocm-7.14-x64`, then `cpu` | Vulkan needs only the driver; ROCm needs its runtime installed |
| Windows ARM64 (Snapdragon) | `cpu`. Optional `xnnpack` to test, likely no coverage on these fused int8 graphs | `win-cpu-arm64`, optional `win-opencl-adreno-arm64`, then `cpu` | measured: gpu Adreno loses generation and end-to-end. Never default the GPU |
| Linux x64 + NVIDIA | `cuda` (CUDA prebuilt lib), then `cpu` | CUDA built from source, `ubuntu-vulkan-x64`, then `cpu` | no CUDA prebuilt on Linux, ship vulkan or offer a source build |
| Linux x64 + AMD | `cpu` | `ubuntu-rocm-7.14-x64`, `ubuntu-vulkan-x64`, then `cpu` | same driver-vs-runtime tradeoff |
| Linux ARM64 (Jetson) | `cuda` (shared-gpu prebuilt), then `cpu` | build with CUDA in the image | desktop ARM CPU boxes default to `cpu` |
| macOS | `coreml` if the prebuilt compiled it, else `cpu` | `macos-arm64` (Metal), then `cpu` | probes first, handful of Macs |
| macOS x64 | `cpu` | `macos-x64`, then `cpu` | no AMD dGPU path worth prebuilts |

Two biases to weigh when the bench is close:

- Streaming ASR feeds 30 ms chunks at up to 33 Hz. Every EP boundary costs a launch and a sync. GPU offload helps big batches, not this cadence. On this Snapdragon the GPU probe ran exactly at cpu speed with launches visible in per-chunk latency. Keep a 10% winning margin rule and the cpu seeded near the top for ASR everywhere.
- UMA iGPUs (Adreno, Apple, Strix Halo) run the model from system RAM. Offload moves compute, not memory. If the CPU is faster at generation, as measured here, the GPU buys nothing. Only discrete-vRAM GPUs change the RAM picture.

## Auto-selection algorithm

Ship a ranked seed list per platform, then let first-run measurement pick the winner. Steps in order:

1. Check env or CLI override first: `ASR_PROVIDER`, `ASR_THREADS`, `NORMALIZER_BACKEND`, `NORMALIZER_DEVICES`. If set, use them and log the source. This is the escape hatch for all heuristics.
2. Read the persistent cache. If it holds a validated winner from this app version and this device set, use it and stop.
3. Enumerate the GPU set. On Windows use the onnxruntime `GetEpDevices` API or a DXGI device hash (vendor, name, adapter id) and store the hash with the winner.
4. For each candidate in the platform seed order, capped at cpu plus two GPUs, do the validity check: capture the recognizer creation log and require that no "Fallback to cpu" line appears. A creation that logged a fallback is rejected outright, no timing needed.
5. Warm up each candidate on a short clip once. GPU first-run cost is real: cuDNN heuristics, DML shader compile, llama Vulkan pipeline cache. Warming seeds the on-disk caches, so the user's first real dictation does not pay them.
6. Cooldown between candidates. This machine throttles back-to-back runs by 2.3-2.6x, one clip once ran past 290 s under thermal skew. Sleep 30 s between candidates, run the suite on a cold machine, and do not bench while on battery or in a multi-day warm room.
7. Time the winner decision. For ASR, decode ~10 s of bundled speech and measure RTF, twice per candidate, take the better. For the normalizer, run a fixed prefill+decode and compare wall time. Accept a GPU only if it beats the cpu run by at least 1.1x. Below that, cpu is the more predictable runtime and the safer default.
8. Persist the winner to config: provider or backend, device list, threads, the device hash, the app version, and the date. The existing `config.json` in `%APPDATA%\amanuensis` carries it, or a sidecar like `backend-cache.json`.
9. Re-bench when any invalidation fires: app version changes the selection code, the device hash changes (external GPU docked, driver update), the user clicks re-detect, or the cached entry is older than the model's update.

Thread count is part of the decision, not a constant. We measured ASR RTF 0.96-1.01x at 2 threads and 0.84-0.87x at 4. Pick 4 threads when thermal headroom allows (desktop, cool), 2 on thin-and-light, and keep `ASR_THREADS` as the override. For the normalizer, threads are only meaningful on cpu; GPU offload ignores them.

## Risks

Silent cpu fallback. Two layers: the wrong-string case in provider.cc and the not-compiled-in case in session.cc, both invisible to `OnlineRecognizer::create`. Every deployment must run the log capture check in step 4, never trust the provider string alone.

Quant-type coverage on OpenCL. q4_k_m loads and runs on the Adreno build, which is what we benched. Newer block quants and several i-quants are unimplemented in OpenCL and Vulkan kernels; llama.cpp warns and falls back per op or refuses the device. Lock the normalizer to q4_k_m or q8_0 GGUF families and treat any other quant as a bench-time experiment.

Thermal throttling poisons benchmarks. Our first batch skewed 2.3-2.6x and mislabeled a 0.6B model as borderline-realtime. Every number in this project's docs was re-taken cool, spaced, single-load. The auto-selection must enforce the same discipline or it will persist a throttled loser.

Warm-up and compile on first GPU run. The first decode on a fresh device pays seconds of EP init. Warm during seeding (step 5), and expect the user's first-ever dictation after a GPU selection to be slower than steady state.

RAM on unified-memory GPUs. A 462 MB normalizer offloaded to the Adreno still lives in system RAM and reports `uma: 1`. GPU choice does not reduce memory pressure on this class of hardware; it only changes where compute happens. On discrete GPUs, size the offload to VRAM minus other apps.

Per-chunk latency on streaming ASR. GPU EPs add per-inference launch overhead that the 33 Hz feed rate exposes. Do not ship a GPU ASR default on any platform without the RTF margin check, and expect this to push most small-model streaming workloads back to cpu.

## Near-future NPU axis (this machine has a Hexagon NPU)

Copilot+ Windows 24H2 can run ONNX on the NPU today, but not through our runtime:

- WinML downloads and registers a QNN EP for Snapdragon X Plus automatically, and the `onnxruntime-qnn` Python/NuGet packages ship NPU inference for Windows ARM64. The QNN EP needs a per-SoC quantized and compiled model, and model.compile is a one-time step cached to disk.
- sherpa-onnx has no QNN path on Windows. Its QNN/HTP support builds for Android phones with adb and phones' models (SM8350 family releases). Nothing for Windows arm64 in the crate or the prebuilts.
- Reaching the NPU means moving ASR off the sherpa crate onto either WinML or a plain onnxruntime build statically linked with the QNN plugin, then re-quantizing the Nemotron graphs for the Hexagon (QNN HTP likes int8/ w8a8). That is a runtime swap, not a flag.

Track the NPU as a separate workstream. There is no defensible path to it while the ASR engine is the pinned sherpa-onnx crate, and we already know the Oryon cpu runs the current models at or under realtime.

## Sources

- sherpa provider map and fallback: https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/provider.cc
- sherpa session creation, EP attach and fallback logs: https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/session.cc
- sherpa v1.13.5 prebuilt libs (win-x64 CUDA, arm64 CPU-only, Jetson GPU): https://github.com/k2-fsa/sherpa-onnx/releases/tag/v1.13.5
- sherpa QNN/Android scope: https://k2-fsa.github.io/sherpa/onnx/qnn/index.html
- onnxruntime EP list and strings: https://onnxruntime.ai/docs/execution-providers/ and the constants header linked from issue 22101
- DirectML sustained-engineering note and x64/x86 build scope: https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html and https://onnxruntime.ai/docs/build/eps.html
- native WebGPU EP on Windows via Dawn/D3D12: https://onnxruntime.ai/docs/execution-providers/WebGPU-ExecutionProvider.html
- auto EP selection, v1.22 planner: https://raven.github.io/ (Olive PR 1854) and onnxruntime PR 24430
- WinML execution providers incl. QNN and device policies: https://learn.microsoft.com/en-us/windows/ai/new-windows-ml/supported-execution-providers and the select-execution-providers / register-execution-providers pages
- onnxruntime QNN EP OS support: https://onnxruntime.ai/docs/execution-providers/QNN-ExecutionProvider.html and https://github.com/microsoft/onnxruntime-qnn
- no arm64 DirectML onnxruntime: https://github.com/microsoft/onnxruntime-genai/issues/637
- llama.cpp build notes, `--device`, `--list-devices`, `GGML_BACKEND_DL`: https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md
- llama.cpp device registry and init_best ordering: https://github.com/ggml-org/llama.cpp/blob/master/ggml/src/ggml-backend-reg.cpp
- Vulkan iGPU hiding and env overrides: PR 15793 and PR 14099 in ggml-org/llama.cpp
- llama.cpp b10675 assets (win-cpu-arm64, win-opencl-adreno-arm64, win-vulkan-x64, ubuntu-rocm, ubuntu-vulkan): https://github.com/ggml-org/llama.cpp/releases/tag/b10675
- whisper.cpp first-GPU-wins flaw and its tracking issue: https://github.com/ggml-org/whisper.cpp/issues/3205
- measured GPU/CPU log for this machine: research/webgpu-s1-vs-nemotron.md