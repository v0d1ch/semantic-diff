use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Use the platform clipboard without passing review text as process arguments.
pub async fn copy_markdown(text: &str) -> Result<()> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip.exe", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    let (path, args) = candidates
        .iter()
        .find_map(|(name, args)| which::which(name).ok().map(|path| (path, *args)))
        .context("No clipboard tool found (use pbcopy, wl-copy, xclip, or xsel)")?;
    let mut command = Command::new(path);
    command.args(args);
    tokio::time::timeout(Duration::from_secs(5), write_clipboard(&mut command, text))
        .await
        .context("Clipboard operation timed out")?
}

async fn write_clipboard(command: &mut Command, text: &str) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("Clipboard stdin unavailable")?;
    let written = stdin.write_all(text.as_bytes()).await;
    drop(stdin);
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "Clipboard command failed: {status}");
    written?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sends_markdown_through_stdin_without_touching_real_clipboard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copied.md");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "cat > \"$1\"", "fake-clipboard"])
            .arg(&path);
        let markdown = "# Review\n\n**Unicode:** čćž\n`$HOME` stays literal.\n";
        write_clipboard(&mut command, markdown).await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), markdown);
    }

    #[tokio::test]
    async fn reports_clipboard_command_failure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 1"]);
        assert!(write_clipboard(&mut command, "review").await.is_err());
    }
}
