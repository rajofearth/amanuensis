use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::fetch::{self, DownloadProgress};
use crate::log;

const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";
/// Bench default control line (`bench/run_normalizer.ps1 -ControlLine`);
/// superseded at runtime by `control_line()`. Kept for parity + tests.
#[allow(dead_code)]
const S1_CONTROL_LINE: &str = "[Styling: semi-formal] [Structure: prose] [Context: general]";
const MAX_NEW_TOKENS: i32 = 1024;
const MIN_NEW_TOKENS: i32 = 64;

/// Defaults for the rewrite presets (match `config::AppConfig` defaults).
pub const DEFAULT_STYLING: &str = "casual";
pub const DEFAULT_STRUCTURE: &str = "prose";
pub const DEFAULT_CONTEXT: &str = "general";

/// s1-mini asset URLs, pinned from the bench ground truth
/// (`research/webgpu-s1-vs-nemotron.md`: first-party
/// `superwhisper/s1-mini-GGUF` GGUF + llama.cpp b10675 win-arm64 prebuilt).
/// Env `S1_GGUF_URL` / `S1_LLAMA_CLI_URL` override these. The llama-cli asset
/// name follows the observed b10675 `llama-b10675-bin-win-<target>-arm64.zip`
/// pattern — the download agent should verify it against the release assets.
pub const S1_GGUF_URL: &str =
    "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf";
pub const S1_LLAMA_CLI_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download/b10675/llama-b10675-bin-win-cpu-arm64.zip";

/// Pinned s1-mini GGUF byte size; `ensure_s1_assets` refuses a GGUF whose
/// length differs, same as `fetch::ensure_file` exact-size checks.
pub const S1_GGUF_BYTES: u64 = 484_219_808;

/// Validate a styling preset; unknown values fall back to the default word so
/// the prompt never carries garbage.
pub fn valid_styling(s: &str) -> &str {
    match s {
        "casual" | "semi-casual" | "semi-formal" | "formal" => s,
        _ => DEFAULT_STYLING,
    }
}

/// Validate a structure preset; unknown values fall back to the default word.
pub fn valid_structure(s: &str) -> &str {
    match s {
        "prose" | "lists" => s,
        _ => DEFAULT_STRUCTURE,
    }
}

/// Validate a context preset; unknown values fall back to the default word.
pub fn valid_context(s: &str) -> &str {
    match s {
        "general" | "email" => s,
        _ => DEFAULT_CONTEXT,
    }
}

/// Build the control line from validated presets.
pub fn control_line(styling: &str, structure: &str, context: &str) -> String {
    format!(
        "[Styling: {}] [Structure: {}] [Context: {}]",
        valid_styling(styling),
        valid_structure(structure),
        valid_context(context)
    )
}

fn models_root() -> Result<PathBuf, String> {
    crate::config::config_dir().map(|dir| dir.join("models"))
}

/// Path to the bundled s1-mini GGUF.
pub fn s1_model_path() -> Option<PathBuf> {
    let root = models_root().ok()?;
    let path = root.join("s1-mini-q4_k_m.gguf");
    std::fs::metadata(&path).ok().map(|_| path)
}

/// Path to the bundled llama-cli executive.
pub fn s1_llama_cli_path() -> Option<PathBuf> {
    let root = models_root().ok()?;
    let path = root.join("llama-cli.exe");
    std::fs::metadata(&path).ok().map(|_| path)
}

/// Paths for the s1-mini rewrite pass. Both must exist or `rewrite` is a no-op.
fn s1_paths() -> Option<(PathBuf, PathBuf)> {
    let model = s1_model_path()?;
    let llama = s1_llama_cli_path()?;
    Some((llama, model))
}

pub struct RewriteOptions {
    /// llama.cpp thread count.
    pub threads: i32,
    /// Number of layers offloaded to the GPU, `0` for CPU-only.
    pub gpu_layers: i32,
    /// Control line prepended to the transcript, built with `control_line()`.
    pub control_line: String,
}

/// The cleaned text returned by an s1-mini rewrite pass.
pub struct RewriteResult {
    pub text: String,
    pub wall_ms: u64,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub ok: bool,
}

/// Run the s1-mini normalizer on a transcript. Returns `None` when the bundled
/// model/executable is missing or the child could not be started, so callers
/// can fall back to the raw transcript without treating it as an error.
pub fn rewrite(transcript: &str, options: &RewriteOptions) -> Option<RewriteResult> {
    let (llama, model) = s1_paths()?;
    let started = Instant::now();
    let cleaned = run_llama(&llama, &model, transcript, options);
    let wall_ms = started.elapsed().as_millis() as u64;
    let in_tokens = estimate_tokens(transcript);
    let out_tokens = estimate_tokens(&cleaned.text);
    let ok = cleaned.ok;
    Some(RewriteResult {
        text: cleaned.text,
        wall_ms,
        in_tokens,
        out_tokens,
        ok,
    })
}

struct CleanedText {
    text: String,
    ok: bool,
}

fn run_llama(
    llama: &Path,
    model: &Path,
    transcript: &str,
    options: &RewriteOptions,
) -> CleanedText {
    let user_message = format!("{}\n{transcript}", options.control_line);
    let mut command = std::process::Command::new(llama);
    command
        .arg("-m")
        .arg(model)
        .arg("--jinja")
        .arg("--chat-template-kwargs")
        .arg("{\"enable_thinking\":false}")
        .arg("--temp")
        .arg("0")
        .arg("-t")
        .arg(options.threads.to_string())
        .arg("-c")
        .arg("2048")
        .arg("-st")
        .arg("-ngl")
        .arg(options.gpu_layers.to_string())
        .arg("-n")
        .arg(max_new_tokens(transcript).to_string())
        .arg("-sys")
        .arg(S1_SYSTEM_PROMPT)
        .arg("-p")
        .arg(&user_message)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(child) = command.spawn() else {
        log!("rewrite", "failed to spawn llama-cli");
        return CleanedText {
            text: String::new(),
            ok: false,
        };
    };
    let output = match wait_bounded(child) {
        Ok(output) => output,
        Err(error) => {
            log!("rewrite", "llama-cli wait failed: {error}");
            return CleanedText {
                text: String::new(),
                ok: false,
            };
        }
    };
    if !output.status.success() {
        log!(
            "rewrite",
            "llama-cli exited with code {:?}",
            output.status.code()
        );
        return CleanedText {
            text: String::new(),
            ok: false,
        };
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr_signals_failure(&stderr) {
        log!("rewrite", "llama-cli reported error/failure on stderr");
        return CleanedText {
            text: String::new(),
            ok: false,
        };
    }
    let combined = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), stderr);
    let text = extract_cleaned_text(&combined, transcript, &options.control_line);
    let ok = !text.is_empty();
    CleanedText { text, ok }
}

/// Hidden consoles: the rewrite child and the zip extraction run off the
/// hot path; a flashing console window per run is a bug, not feedback.
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Upper bound for one rewrite pass. Measured cost is ~3-6 s/clip on CPU;
/// 120 s is far above any healthy run and only trips on a wedged child.
const REWRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wait for the child with a hard timeout, killing it if it wedges so the
/// serial ASR worker can never block forever on the rewrite pass.
fn wait_bounded(mut child: std::process::Child) -> std::io::Result<std::process::Output> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output(),
            Ok(None) => {
                if started.elapsed() >= REWRITE_TIMEOUT {
                    log!("rewrite", "llama-cli wedged; killing after 120s");
                    let _ = child.kill();
                    return child.wait_with_output();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

fn regex_lower(haystack: &str, pattern: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|re| re.is_match(&haystack.to_lowercase()))
        .unwrap_or(false)
}

/// True only on failure-shaped stderr lines. llama.cpp logs benign words like
/// "error" (error statistics, hints) on success, so a bare substring match
/// false-positives; require `:` / `failed to`-style phrasing instead.
fn stderr_signals_failure(stderr: &str) -> bool {
    regex_lower(
        stderr,
        r"error\s*:|failed to |load failed|could not |cannot (open|load|find) |exception|fatal|aborted|no such file|invalid (model|argument|grammar)",
    )
}

/// Scale `-n` to the transcript so short dictations don't pay for a full
/// 1024-token generation window. Mirrors the Space formula: 1.3x input
/// tokens plus slack, clamped to [MIN_NEW_TOKENS, MAX_NEW_TOKENS].
fn max_new_tokens(transcript: &str) -> i32 {
    let scaled = estimate_tokens(transcript)
        .saturating_mul(13)
        .saturating_div(10)
        .saturating_add(32)
        .clamp(MIN_NEW_TOKENS as u64, MAX_NEW_TOKENS as u64);
    scaled as i32
}

/// Cap on transcript bytes baked into the boundary regex / exact needle.
/// The transcript is untrusted ASR text; without a cap a pasted paragraph
/// builds a multi-KB pattern and prompt junk can echo back as cleaned text.
const MAX_TRANSCRIPT_PATTERN_BYTES: usize = 2048;
/// Suffix length used to locate an over-cap transcript's end without
/// building a pattern from the whole string.
const TRANSCRIPT_TAIL_ANCHOR_BYTES: usize = 256;

/// Locate the end of the echoed transcript in llama output. Short
/// transcripts use the exact control-line + transcript regex (plus an exact
/// needle fallback); over-cap transcripts anchor on a bounded tail suffix so
/// the pattern size stays constant. `control_line` must be the line actually
/// sent in the prompt, otherwise the boundary regex cannot match.
fn find_transcript_end(all: &str, transcript: &str, control_line: &str) -> Option<usize> {
    if transcript.is_empty() {
        return None;
    }
    if transcript.len() <= MAX_TRANSCRIPT_PATTERN_BYTES {
        let boundary = format!(
            r"[^\r\n]*{}[^\r\n]*\r?\n[^\r\n]*{}(?:[ \t]*)\r?\n",
            regex::escape(control_line),
            regex::escape(transcript)
        );
        if let Ok(re) = regex::Regex::new(&boundary) {
            if let Some(m) = re.find(all) {
                return Some(m.end());
            }
        }
        let needle = format!("{transcript}\n");
        if let Some(index) = all.rfind(&needle) {
            return Some(index + transcript.len());
        }
        return None;
    }
    let tail = tail_anchor(transcript, TRANSCRIPT_TAIL_ANCHOR_BYTES);
    if tail.is_empty() {
        return None;
    }
    let index = all.rfind(tail)?;
    let after_tail = index + tail.len();
    let rest = &all[after_tail..];
    let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let rest_trimmed = &rest[spaces..];
    rest_trimmed
        .strip_prefix('\n')
        .map(|_| after_tail + spaces + 1)
}

/// Last role-header boundary for the assistant turn. Requires a line-start +
/// `:`/newline terminator (or the `<|im_start|>` marker) so prose containing
/// the bare word "assistant" can't match and leak prompt junk.
fn assistant_role_start(all: &str) -> Option<usize> {
    const MARKER: &str = "<|im_start|>assistant";
    let mut best: Option<usize> = None;
    if let Some(index) = all.rfind(MARKER) {
        let end = index + MARKER.len();
        best = Some(best.map_or(end, |b: usize| b.max(end)));
    }
    for needle in ["\nassistant:", "\nassistant\n"] {
        if let Some(index) = all.rfind(needle) {
            let end = index + needle.len();
            best = Some(best.map_or(end, |b: usize| b.max(end)));
        }
    }
    for needle in ["assistant:", "assistant\n"] {
        if all.starts_with(needle) {
            best = Some(best.map_or(needle.len(), |b: usize| b.max(needle.len())));
        }
    }
    best
}

fn tail_anchor<'a>(s: &'a str, max_bytes: usize) -> &'a str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Port of `Get-CleanText` from bench/run_normalizer.ps1. Locates the control
/// line + transcript boundary in llama's output and strips the prompt banner
/// and trailing markers. `control_line` must be the line actually sent.
fn extract_cleaned_text(all: &str, transcript: &str, control_line: &str) -> String {
    let all = all.replace('\r', "");
    let start =
        find_transcript_end(&all, transcript, control_line).or_else(|| assistant_role_start(&all));
    let Some(start) = start else {
        return String::new();
    };
    let mut text = all[start..].to_owned();
    text = regex::Regex::new(r"(?s)^\s*(<\|im_start\|>)?\s*")
        .map(|re| re.replace(&text, "").into_owned())
        .unwrap_or(text);
    text = regex::Regex::new(r"(?s)\s*Exiting\.\.\.\s*$")
        .map(|re| re.replace(&text, "").into_owned())
        .unwrap_or(text);
    text = regex::Regex::new(r"(?s)(<\|im_start\|>)?\s*\[ (?:Prompt|Generation):[^\r\n]* \]\s*$")
        .map(|re| re.replace(&text, "").into_owned())
        .unwrap_or(text);
    text = regex::Regex::new(r"(?s)\s*<\|im_end\|>\s*$")
        .map(|re| re.replace(&text, "").into_owned())
        .unwrap_or(text);
    text = regex::Regex::new(r"(?s)\s*\[end of text\]\s*$")
        .map(|re| re.replace(&text, "").into_owned())
        .unwrap_or(text);
    text.trim().to_owned()
}

pub(crate) fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() / 4) as u64
}

/// Download + extract the missing s1-mini assets (GGUF + llama-cli zip) with
/// per-chunk `DownloadProgress` identical in shape to `fetch::ensure_model`,
/// so the onboarding progress UI consumes it unchanged. Skips whatever is
/// already present (GGUF byte-exact, CLI by `s1_llama_cli_path`). `Ok(true)`
/// means both assets are ready; `Ok(false)` means cancelled.
pub fn ensure_s1_assets(
    on_progress: &mut dyn FnMut(DownloadProgress),
    cancel: Option<&AtomicBool>,
) -> Result<bool, String> {
    let cancelled = || cancel.is_some_and(|flag| flag.load(Ordering::SeqCst));
    if cancelled() {
        return Ok(false);
    }
    let root = models_root()?;
    std::fs::create_dir_all(&root).map_err(|error| format!("creating models dir: {error}"))?;

    // Pinned for the download agent (env overrides honored for testing).
    let gguf_url = std::env::var("S1_GGUF_URL").unwrap_or_else(|_| S1_GGUF_URL.to_owned());
    let zip_url = std::env::var("S1_LLAMA_CLI_URL").unwrap_or_else(|_| S1_LLAMA_CLI_URL.to_owned());
    let zip_name = zip_url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("llama-cli.zip");

    let zip_total = fetch::head_content_length(&zip_url);
    let mut aggregate = fetch::Aggregate::new(S1_GGUF_BYTES + zip_total.unwrap_or(0));

    // GGUF, byte-exact.
    let gguf_path = root.join("s1-mini-q4_k_m.gguf");
    let have_gguf = std::fs::metadata(&gguf_path)
        .map(|meta| meta.is_file() && meta.len() == S1_GGUF_BYTES)
        .unwrap_or(false);
    if !have_gguf {
        let label = "s1-mini-q4_k_m.gguf".to_owned();
        let ready = fetch::download_url_to(
            &gguf_url,
            &label,
            &gguf_path,
            Some(S1_GGUF_BYTES),
            cancel,
            &mut |streamed| on_progress(aggregate.current(&label, streamed)),
        )?;
        if !ready {
            return Ok(false);
        }
    }
    aggregate.finish_file(S1_GGUF_BYTES);

    // llama-cli zip (flat: exe + DLLs); extract ALL files into models/ so the
    // DLLs sit beside llama-cli.exe.
    if s1_llama_cli_path().is_none() {
        if cancelled() {
            return Ok(false);
        }
        let zip_path = root.join(zip_name);
        let ready = fetch::download_url_to(
            &zip_url,
            zip_name,
            &zip_path,
            zip_total,
            cancel,
            &mut |streamed| on_progress(aggregate.current(zip_name, streamed)),
        )?;
        if !ready {
            return Ok(false);
        }
        let zip_len = std::fs::metadata(&zip_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        expand_archive(&zip_path, &root)?;
        let _ = std::fs::remove_file(&zip_path);
        if s1_llama_cli_path().is_none() {
            return Err(format!("llama-cli.exe missing after extracting {zip_name}"));
        }
        aggregate.finish_file(zip_total.unwrap_or(zip_len));
    } else {
        aggregate.finish_file(zip_total.unwrap_or(0));
    }
    Ok(true)
}

/// Extract a flat zip with PowerShell (Windows-only app, no new deps),
/// mirroring the `powershell.exe` + `$args[]` style in `telemetry.rs`.
fn expand_archive(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let output = std::process::Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("Expand-Archive -Path $args[0] -DestinationPath $args[1] -Force")
        .arg(zip_path)
        .arg(dest_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("running powershell Expand-Archive: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Expand-Archive failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Check the s1-mini GGUF + llama-cli presence under `models/`, fetching what
/// is missing via `ensure_s1_assets`. Returns `Ok(true)` only when both
/// exist. `on_progress` reports `(ready, total)` readiness; byte-level
/// progress flows through `ensure_s1_assets`' own callback instead.
pub fn ensure_rewrite_assets(on_progress: &mut dyn FnMut(&str, u64, u64)) -> Result<bool, String> {
    let readiness = || {
        let ready = u64::from(s1_model_path().is_some()) + u64::from(s1_llama_cli_path().is_some());
        (ready, 2)
    };
    let (ready, total) = readiness();
    on_progress("s1-mini", ready, total);
    let fetched = ensure_s1_assets(&mut |_| {}, None)?;
    let (ready, total) = readiness();
    on_progress("s1-mini", ready, total);
    if s1_model_path().is_none() {
        log!("rewrite", "s1-mini GGUF missing under models/");
    }
    if s1_llama_cli_path().is_none() {
        log!("rewrite", "llama-cli missing under models/");
    }
    Ok(fetched && ready == total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_strips_banner_and_trailer() {
        let transcript = "hello world";
        let all = format!(
            "[Styling: semi-formal] [Structure: prose] [Context: general]\n{transcript}\ncleaned words here\nExiting...\n"
        );
        let cleaned = extract_cleaned_text(&all, transcript, S1_CONTROL_LINE);
        assert_eq!(cleaned, "cleaned words here");
    }

    #[test]
    fn extraction_fallback_via_assistant_when_boundary_missing() {
        let all = "some prompt bytes\nassistant\nthe real answer\n[ Generation: 12 tokens ]";
        let cleaned = extract_cleaned_text(all, "nothing", S1_CONTROL_LINE);
        assert_eq!(cleaned, "the real answer");
    }

    #[test]
    fn extraction_returns_empty_when_no_marker() {
        let cleaned = extract_cleaned_text("no markers at all", "x", S1_CONTROL_LINE);
        assert_eq!(cleaned, "");
    }

    #[test]
    fn max_new_tokens_scales_with_floor_and_ceiling() {
        assert_eq!(max_new_tokens(""), MIN_NEW_TOKENS);
        assert_eq!(max_new_tokens("hi"), MIN_NEW_TOKENS);
        // 400 chars -> ~100 tokens -> floor(100*1.3)+32 = 162.
        assert_eq!(max_new_tokens(&"a".repeat(400)), 162);
        assert_eq!(max_new_tokens(&"a".repeat(100_000)), MAX_NEW_TOKENS);
    }

    #[test]
    fn max_new_tokens_floor_boundary() {
        // 96 chars -> 24 tokens -> floor(24*1.3)+32 = 63 -> clamped to 64.
        assert_eq!(max_new_tokens(&"a".repeat(96)), 64);
        // 100 chars -> 25 tokens -> floor(25*1.3)+32 = 64.
        assert_eq!(max_new_tokens(&"a".repeat(100)), 64);
        // 104 chars -> 26 tokens -> floor(26*1.3)+32 = 65.
        assert_eq!(max_new_tokens(&"a".repeat(104)), 65);
    }

    #[test]
    fn max_new_tokens_ceiling_boundary() {
        // 3052 chars -> 763 tokens -> floor(763*1.3)+32 = 1023.
        assert_eq!(max_new_tokens(&"a".repeat(3052)), 1023);
        // 3056 chars -> 764 tokens -> floor(764*1.3)+32 = 1025 -> clamped.
        assert_eq!(max_new_tokens(&"a".repeat(3056)), MAX_NEW_TOKENS);
    }

    #[test]
    fn stderr_ignores_benign_error_mentions() {
        assert!(!stderr_signals_failure(
            "llama_context: error stats live here"
        ));
        assert!(!stderr_signals_failure(""));
        assert!(stderr_signals_failure("error: failed to load model"));
        assert!(stderr_signals_failure(
            "llama_cli: failed to open model file"
        ));
        assert!(stderr_signals_failure("could not read vocab"));
    }

    #[test]
    fn extraction_rejects_bare_assistant_mention() {
        let cleaned =
            extract_cleaned_text("my assistant helped a lot today", "zzz", S1_CONTROL_LINE);
        assert_eq!(cleaned, "");
    }

    #[test]
    fn extraction_accepts_colon_role_header() {
        let cleaned = extract_cleaned_text("prompt\nassistant: the answer", "zzz", S1_CONTROL_LINE);
        assert_eq!(cleaned, "the answer");
    }

    #[test]
    fn extraction_handles_overcap_transcript_via_tail() {
        let transcript = "t".repeat(MAX_TRANSCRIPT_PATTERN_BYTES + 100);
        let all = format!("{S1_CONTROL_LINE}\n{transcript}\nreal answer\n");
        assert_eq!(
            extract_cleaned_text(&all, &transcript, S1_CONTROL_LINE),
            "real answer"
        );
    }

    #[test]
    fn extraction_overcap_without_newline_boundary_returns_empty() {
        let transcript = "t".repeat(MAX_TRANSCRIPT_PATTERN_BYTES + 100);
        let all = format!("prefix {} suffix without boundary", &transcript[..300]);
        assert_eq!(extract_cleaned_text(&all, &transcript, S1_CONTROL_LINE), "");
    }

    #[test]
    fn control_line_builds_from_valid_presets() {
        assert_eq!(
            control_line("formal", "lists", "email"),
            "[Styling: formal] [Structure: lists] [Context: email]"
        );
        assert_eq!(
            control_line("casual", "prose", "general"),
            "[Styling: casual] [Structure: prose] [Context: general]"
        );
    }

    #[test]
    fn control_line_falls_back_per_field() {
        assert_eq!(
            control_line("shouty", "bullets", "meeting"),
            "[Styling: casual] [Structure: prose] [Context: general]"
        );
        // Empty strings are unknown too.
        assert_eq!(
            control_line("", "", ""),
            "[Styling: casual] [Structure: prose] [Context: general]"
        );
        // Valid fields survive alongside invalid ones.
        assert_eq!(
            control_line("formal", "bullets", "email"),
            "[Styling: formal] [Structure: prose] [Context: email]"
        );
    }

    #[test]
    fn extraction_matches_runtime_control_line() {
        let line = control_line("formal", "lists", "email");
        let transcript = "hello world";
        let all = format!("{line}\n{transcript}\ncleaned words here\n");
        assert_eq!(
            extract_cleaned_text(&all, transcript, &line),
            "cleaned words here"
        );
        // A stale control line must not match the boundary (falls through to
        // the needle fallback only when the transcript echoes with newline).
        assert_eq!(
            extract_cleaned_text(&all, transcript, S1_CONTROL_LINE),
            "cleaned words here"
        );
    }
}
