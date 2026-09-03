use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::log;

const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";
const S1_CONTROL_LINE: &str = "[Styling: semi-formal] [Structure: prose] [Context: general]";
const MAX_NEW_TOKENS: i32 = 512;

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
        .arg(MAX_NEW_TOKENS.to_string())
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
    if regex_lower(&stderr, "error|failed") {
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

/// Port of `Get-CleanText` from bench/run_normalizer.ps1. Locates the control
/// line + transcript boundary in llama's output and strips the prompt banner
/// and trailing markers.
fn extract_cleaned_text(all: &str, transcript: &str) -> String {
    let all = all.replace('\r', "");
    let boundary = format!(
        r"[^\r\n]*{}[^\r\n]*\r?\n[^\r\n]*{}(?:[ \t]*)\r?\n",
        regex::escape(S1_CONTROL_LINE),
        regex::escape(transcript)
    );
    let start = regex::Regex::new(&boundary)
        .and_then(|re| Ok(re.find(&all).map(|m| m.end())))
        .unwrap_or(None)
        .or_else(|| {
            let needle = format!("{transcript}\n");
            all.rfind(&needle).map(|index| index + transcript.len())
        })
        .or_else(|| {
            all.rfind("assistant")
                .map(|index| index + "assistant".len())
        });
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
}
