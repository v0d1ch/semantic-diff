//! Mermaid code block extraction, rendering via mmdc, and content-hash caching.
//!
//! Renders mermaid diagrams as halfblock images when:
//! 1. mmdc is installed
//! 2. Terminal supports inline images (Ghostty, iTerm2, Kitty)
//! 3. NOT inside a multiplexer (tmux) which strips graphics escape sequences
//!
//! Falls back to styled source code otherwise.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A mermaid code block extracted from markdown.
#[derive(Debug, Clone)]
pub struct MermaidBlock {
    /// Raw mermaid source code.
    pub source: String,
    /// Blake3 content hash of the source.
    pub hash: String,
}

impl MermaidBlock {
    pub fn new(source: String) -> Self {
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        Self { source, hash }
    }
}

/// State of a mermaid diagram render.
#[derive(Debug, Clone)]
pub enum MermaidRenderState {
    Pending,
    Rendering,
    Ready(PathBuf),
    Failed(String),
}

/// Whether mermaid image rendering is supported in the current environment.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageSupport {
    /// Terminal supports inline images via native protocol.
    Supported(ImageProtocol),
    /// Inside a multiplexer or unsupported terminal — show styled source.
    Multiplexer,
    /// mmdc is not installed — show styled source.
    NoMmdc,
}

/// Which inline image protocol the terminal supports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageProtocol {
    /// iTerm2 inline image protocol (OSC 1337).
    Iterm2,
    /// Kitty graphics protocol.
    Kitty,
}

fn terminal_protocol(term_program: &str, ghostty: bool) -> Option<ImageProtocol> {
    let name = term_program.to_ascii_lowercase();

    if name.contains("iterm") {
        Some(ImageProtocol::Iterm2)
    } else if ghostty || name.contains("kitty") || name.contains("wezterm") || name == "rex" {
        Some(ImageProtocol::Kitty)
    } else {
        None
    }
}

/// Detect whether the current terminal environment supports inline image rendering.
///
/// Checks are ordered: multiplexer detection first (these strip graphics escape
/// sequences), then terminal capability, then mmdc availability.
///
/// Key distinction: we check for **active session** env vars, not just installed software.
/// - `TMUX` is only set inside a tmux session (not just because tmux is installed)
/// - `STY` is only set inside a screen session
pub fn detect_image_support() -> ImageSupport {
    if std::env::var("TMUX").is_ok() || std::env::var("STY").is_ok() {
        tracing::info!("Inside tmux or screen — Mermaid images disabled");
        return ImageSupport::Multiplexer;
    }

    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let ghostty = std::env::var("GHOSTTY_RESOURCES_DIR").is_ok();

    let protocol = match terminal_protocol(&term_program, ghostty) {
        Some(protocol) => protocol,
        None => {
            tracing::info!(
                term_program = %term_program,
                "Unknown terminal — Mermaid images disabled"
            );
            return ImageSupport::Multiplexer;
        }
    };

    if which::which("mmdc").is_err() {
        tracing::info!("mmdc not found — Mermaid diagrams will show as source");
        return ImageSupport::NoMmdc;
    }

    tracing::info!(?protocol, "Image rendering enabled");
    ImageSupport::Supported(protocol)
}

/// Cache and state manager for mermaid diagram rendering.
pub struct MermaidCache {
    pub states: Arc<Mutex<HashMap<String, MermaidRenderState>>>,
    cache_dir: Option<PathBuf>,
    mmdc_available: bool,
}

impl Default for MermaidCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MermaidCache {
    pub fn new() -> Self {
        let cache_dir = find_cache_dir();
        let mmdc_available = which::which("mmdc").is_ok();

        let mut initial_states = HashMap::new();

        // Pre-load existing cached PNGs
        if let Some(ref dir) = cache_dir {
            if dir.exists() {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "png") {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                initial_states.insert(
                                    stem.to_string(),
                                    MermaidRenderState::Ready(path.clone()),
                                );
                            }
                        }
                    }
                }
            }
        }

        Self {
            states: Arc::new(Mutex::new(initial_states)),
            cache_dir,
            mmdc_available,
        }
    }

    /// Get the render state synchronously.
    pub fn get_state_blocking(&self, hash: &str) -> MermaidRenderState {
        let states = self.states.lock().unwrap();
        states
            .get(hash)
            .cloned()
            .unwrap_or(MermaidRenderState::Pending)
    }

    /// Spawn async rendering of a mermaid block via mmdc.
    pub fn render_async(
        &self,
        block: MermaidBlock,
        tx: tokio::sync::mpsc::Sender<crate::app::Message>,
    ) {
        if !self.mmdc_available {
            let mut states = self.states.lock().unwrap();
            states.insert(
                block.hash.clone(),
                MermaidRenderState::Failed("mmdc not installed".to_string()),
            );
            return;
        }

        // Check if already rendering or ready
        {
            let states = self.states.lock().unwrap();
            match states.get(&block.hash) {
                Some(MermaidRenderState::Ready(_)) | Some(MermaidRenderState::Rendering) => return,
                _ => {}
            }
        }

        // Mark as rendering
        {
            let mut states = self.states.lock().unwrap();
            states.insert(block.hash.clone(), MermaidRenderState::Rendering);
        }

        let cache_dir = match &self.cache_dir {
            Some(d) => d.clone(),
            None => {
                let mut states = self.states.lock().unwrap();
                states.insert(
                    block.hash.clone(),
                    MermaidRenderState::Failed("No git directory found".to_string()),
                );
                return;
            }
        };

        let states = self.states.clone();
        let hash = block.hash.clone();

        tokio::spawn(async move {
            let output_path = cache_dir.join(format!("{hash}.png"));

            if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
                let mut s = states.lock().unwrap();
                s.insert(hash, MermaidRenderState::Failed(e.to_string()));
                return;
            }

            let input_path = cache_dir.join(format!("{hash}.mmd"));
            if let Err(e) = tokio::fs::write(&input_path, &block.source).await {
                let mut s = states.lock().unwrap();
                s.insert(hash, MermaidRenderState::Failed(e.to_string()));
                return;
            }

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::process::Command::new("mmdc")
                    .arg("-i")
                    .arg(&input_path)
                    .arg("-o")
                    .arg(&output_path)
                    .arg("-b")
                    .arg("transparent")
                    .arg("-w")
                    .arg("800")
                    .arg("--quiet")
                    .output(),
            )
            .await;

            let _ = tokio::fs::remove_file(&input_path).await;

            match result {
                Ok(Ok(output)) if output.status.success() && output_path.exists() => {
                    let mut s = states.lock().unwrap();
                    s.insert(hash, MermaidRenderState::Ready(output_path));
                }
                Ok(Ok(output)) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let mut s = states.lock().unwrap();
                    s.insert(
                        hash,
                        MermaidRenderState::Failed(format!("mmdc failed: {stderr}")),
                    );
                }
                Ok(Err(e)) => {
                    let mut s = states.lock().unwrap();
                    s.insert(hash, MermaidRenderState::Failed(e.to_string()));
                }
                Err(_) => {
                    let mut s = states.lock().unwrap();
                    s.insert(
                        hash,
                        MermaidRenderState::Failed("mmdc timed out (15s)".to_string()),
                    );
                }
            }

            let _ = tx.send(crate::app::Message::MermaidReady).await;
        });
    }
}

fn find_cache_dir() -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(
        PathBuf::from(git_dir)
            .join("semantic-diff-cache")
            .join("mermaid"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rex_uses_kitty_graphics() {
        assert_eq!(terminal_protocol("rex", false), Some(ImageProtocol::Kitty));
        assert_eq!(terminal_protocol("Rex", false), Some(ImageProtocol::Kitty));
    }

    #[test]
    fn unrelated_terminal_name_is_not_recognized() {
        assert_eq!(terminal_protocol("not-rex", false), None);
    }
}
