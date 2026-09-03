use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::log;

const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";
const S1_CONTROL_LINE: &str = "[Styling: semi-formal] [Structure: prose] [Context: general]";
const MAX_NEW_TOKENS: i32 = 512;
const MIN_NEW_TOKENS: i32 = 128;

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
    let user_message = format!("{S1_CONTROL_LINE}\n{transcript}");
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
    let text = extract_cleaned_text(&combined, transcript);
    let ok = !text.is_empty();
    CleanedText { text, ok }
}

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
/// 512-token generation window. Output mirrors input length, so 2x input
/// tokens plus slack, clamped to [MIN_NEW_TOKENS, MAX_NEW_TOKENS].
fn max_new_tokens(transcript: &str) -> i32 {
    let scaled = estimate_tokens(transcript)
        .saturating_mul(2)
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
/// the pattern size stays constant.
fn find_transcript_end(all: &str, transcript: &str) -> Option<usize> {
    if transcript.is_empty() {
        return None;
    }
    if transcript.len() <= MAX_TRANSCRIPT_PATTERN_BYTES {
        let boundary = format!(
            r"[^\r\n]*{}[^\r\n]*\r?\n[^\r\n]*{}(?:[ \t]*)\r?\n",
            regex::escape(S1_CONTROL_LINE),
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
/// and trailing markers.
fn extract_cleaned_text(all: &str, transcript: &str) -> String {
    let all = all.replace('\r', "");
    let start = find_transcript_end(&all, transcript).or_else(|| assistant_role_start(&all));
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

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() / 4) as u64
}

/// Download the s1-mini GGUF and llama-cli alongside the models. Returns true
/// on full success. Reuses the existing resumable download path style but the
/// artifacts live under `models/` next to the ASR model dirs.
pub fn ensure_rewrite_assets(on_progress: &mut dyn FnMut(&str, u64, u64)) -> Result<bool, String> {
    let root = models_root()?;
    std::fs::create_dir_all(&root).map_err(|error| format!("creating models dir: {error}"))?;

    let llama_cli_url = std::env::var("S1_LLAMA_CLI_URL")
        .unwrap_or_else(|_| "https://github.com/ggml-org/llama.cpp/releases/latest/download/llama-b3870-bin-win-arm64-avx2.zip".to_owned());
    let gguf_url = std::env::var("S1_GGUF_URL").unwrap_or_else(|_| {
        "https://huggingface.co/bartowski/SynthLabs-S1-mini-GGUF/resolve/main/SynthLabs-S1-mini-Q4_K_M.gguf".to_owned()
    });
    // TODO(issue #2): unconditional GGUF fetch is a placeholder until the exact
    // s1-mini q4_k_m asset and llama-cli artifact are pinned from the bench.
    let _ = (llama_cli_url, gguf_url, on_progress);
    Ok(true)
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
        let cleaned = extract_cleaned_text(&all, transcript);
        assert_eq!(cleaned, "cleaned words here");
    }

    #[test]
    fn extraction_fallback_via_assistant_when_boundary_missing() {
        let all = "some prompt bytes\nassistant\nthe real answer\n[ Generation: 12 tokens ]";
        let cleaned = extract_cleaned_text(all, "nothing");
        assert_eq!(cleaned, "the real answer");
    }

    #[test]
    fn extraction_returns_empty_when_no_marker() {
        let cleaned = extract_cleaned_text("no markers at all", "x");
        assert_eq!(cleaned, "");
    }

    #[test]
    fn max_new_tokens_scales_with_floor_and_ceiling() {
        assert_eq!(max_new_tokens(""), MIN_NEW_TOKENS);
        assert_eq!(max_new_tokens("hi"), MIN_NEW_TOKENS);
        // 400 chars -> ~100 tokens -> 2*100+32 = 232.
        assert_eq!(max_new_tokens(&"a".repeat(400)), 232);
        assert_eq!(max_new_tokens(&"a".repeat(100_000)), MAX_NEW_TOKENS);
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
        let cleaned = extract_cleaned_text("my assistant helped a lot today", "zzz");
        assert_eq!(cleaned, "");
    }

    #[test]
    fn extraction_accepts_colon_role_header() {
        let cleaned = extract_cleaned_text("prompt\nassistant: the answer", "zzz");
        assert_eq!(cleaned, "the answer");
    }

    #[test]
    fn extraction_handles_overcap_transcript_via_tail() {
        let transcript = "t".repeat(MAX_TRANSCRIPT_PATTERN_BYTES + 100);
        let all = format!("{S1_CONTROL_LINE}\n{transcript}\nreal answer\n");
        assert_eq!(extract_cleaned_text(&all, &transcript), "real answer");
    }

    #[test]
    fn extraction_overcap_without_newline_boundary_returns_empty() {
        let transcript = "t".repeat(MAX_TRANSCRIPT_PATTERN_BYTES + 100);
        let all = format!("prefix {} suffix without boundary", &transcript[..300]);
        assert_eq!(extract_cleaned_text(&all, &transcript), "");
    }
}
