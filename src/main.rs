mod app;
mod cache;
mod cli;
mod config;
mod diff;
mod event;
mod grouper;
mod highlight;
mod preview;
mod review;
mod signal;
mod theme;
mod ui;

use anyhow::{Context, Result};
use app::{App, Command, Message};
use clap::Parser;
use review::ReviewSource;
use tokio::sync::mpsc;

/// Spawn review tasks for a SpawnReviewBatch command.
fn spawn_review_batch(cmd: Command, tx: &mpsc::Sender<Message>, app: &mut App) {
    if let Command::SpawnReviewBatch(cmds) = cmd {
        for cmd in cmds {
            if let Command::SpawnReviewSection {
                backend,
                model,
                reasoning_effort,
                section,
                prompt,
                group_content_hash,
            } = cmd
            {
                let tx2 = tx.clone();
                let handle = tokio::spawn(async move {
                    let result = crate::review::llm::invoke_review_section(
                        backend,
                        &model,
                        reasoning_effort.as_deref(),
                        &prompt,
                    )
                    .await;
                    let _ = tx2
                        .send(Message::ReviewSectionReady(
                            group_content_hash,
                            section,
                            result,
                        ))
                        .await;
                });
                app.review_handles
                    .insert((group_content_hash, section), handle);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Panic hook FIRST (ROB-01) — restore terminal and clean up PID file before printing panic info
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        signal::remove_pid_file();
        ratatui::restore();
        original_hook(info);
    }));

    // 2. Init logging to file (secure directory, not world-writable /tmp/)
    let log_path = signal::log_file_path();
    // Ensure parent directory exists (write_pid_file also does this, but log init comes first)
    if let Some(parent) = log_path.parent() {
        if !parent.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent);
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::create_dir_all(parent);
            }
        }
    }
    let log_file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&log_path)?
    };
    tracing_subscriber::fmt()
        .with_env_filter("semantic_diff=debug")
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!("semantic-diff starting");

    // 2b. Parse CLI arguments
    let cli = cli::Cli::parse();
    if let Some(path) = &cli.review_file {
        let instructions = std::fs::read_to_string(path)
            .with_context(|| format!("Could not read review instructions {}", path.display()))?;
        anyhow::ensure!(
            !instructions.trim().is_empty(),
            "Review instructions are empty: {}",
            path.display()
        );
    }
    let git_diff_args = cli.git_diff_args();
    tracing::info!(?git_diff_args, "Git diff args");

    // 2c. Load config (creates default if missing), CLI --theme overrides config
    let mut config = config::load();
    if cli.theme.is_some() {
        config.theme_mode = cli.theme_mode();
    }
    tracing::info!(?config, "Loaded config");

    // 3. Run git diff with user-specified args (or default: unstaged changes)
    let output = std::process::Command::new("git")
        .args(&git_diff_args)
        .output()?;

    let raw_git_diff = String::from_utf8_lossy(&output.stdout);

    // 4. Parse diff, including untracked files
    let (diff_data, _raw_diff) = diff::parse_with_untracked(&raw_git_diff);

    if diff_data.files.is_empty() && diff_data.binary_files.is_empty() {
        eprintln!("No changes detected");
        return Ok(());
    }
    tracing::info!(
        files = diff_data.files.len(),
        binary = diff_data.binary_files.len(),
        "Parsed diff"
    );

    // 5. Write PID file for external signal senders
    signal::write_pid_file()?;

    // 6. Set up async channel and app
    let (tx, mut rx) = mpsc::channel::<Message>(32);
    let mut app = App::new(diff_data, &config, git_diff_args);

    if let Some(review_file) = cli.review_file {
        let name = review_file
            .file_name()
            .unwrap_or(review_file.as_os_str())
            .to_string_lossy()
            .into_owned();
        app.review_source = ReviewSource::Skill {
            path: review_file,
            name,
        };
    }

    app.event_tx = Some(tx.clone());

    // 7. Spawn the async event loop (terminal events + SIGUSR1)
    tokio::spawn(event::event_loop(tx.clone()));

    // 8. Init terminal and enter main loop
    let mut terminal = ratatui::init();

    let mut had_images_last_frame = false;

    loop {
        let mut pending_images = Vec::new();
        terminal.draw(|f| {
            app.ui_state.viewport_height = f.area().height.saturating_sub(1);
            pending_images = app.view(f);
        })?;

        let has_images = !pending_images.is_empty();

        if let preview::mermaid::ImageSupport::Supported(protocol) = &app.image_support {
            if has_images {
                ui::preview_view::flush_images(&pending_images, *protocol);
            } else if had_images_last_frame {
                // Switched away from preview or to a non-image file — clear stale images
                // and immediately redraw so the screen doesn't flash blank.
                ui::preview_view::clear_stale_images(*protocol, &mut terminal);
                terminal.draw(|f| {
                    app.ui_state.viewport_height = f.area().height.saturating_sub(1);
                    app.view(f);
                })?;
            }
        }
        had_images_last_frame = has_images;

        if let Some(msg) = rx.recv().await {
            if let Some(cmd) = app.update(msg) {
                match cmd {
                    Command::CopyReview(markdown) => {
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            let result = review::clipboard::copy_markdown(&markdown)
                                .await
                                .map_err(|error| error.to_string());
                            let _ = tx2.send(Message::ReviewCopied(result)).await;
                        });
                    }
                    Command::SpawnDiffParse {
                        git_diff_args,
                        purpose,
                    } => {
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            let output = tokio::process::Command::new("git")
                                .args(&git_diff_args)
                                .output()
                                .await;
                            let message = match output {
                                Ok(output) if output.status.success() => {
                                    let raw_git =
                                        String::from_utf8_lossy(&output.stdout).to_string();
                                    let untracked =
                                        crate::diff::untracked::discover_untracked_files_async()
                                            .await;
                                    let (data, combined) = crate::diff::parse_with_untracked_paths(
                                        &raw_git, &untracked,
                                    );
                                    Message::DiffParsed(data, combined, purpose)
                                }
                                Ok(output) => Message::DiffParseFailed(
                                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                                ),
                                Err(error) => Message::DiffParseFailed(error.to_string()),
                            };
                            let _ = tx2.send(message).await;
                        });
                    }
                    Command::SpawnGrouping {
                        backend,
                        model,
                        reasoning_effort,
                        summaries,
                        diff_hash,
                        ..
                    } => {
                        let tx2 = tx.clone();
                        let handle = tokio::spawn(async move {
                            match crate::grouper::llm::request_grouping_with_timeout(
                                backend,
                                &model,
                                reasoning_effort.as_deref(),
                                &summaries,
                            )
                            .await
                            {
                                Ok(groups) => {
                                    // Don't save here — GroupingComplete handler saves with full incremental state
                                    let _ = tx2
                                        .send(Message::GroupingComplete(groups, diff_hash))
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx2
                                        .send(Message::GroupingFailed(e.to_string(), diff_hash))
                                        .await;
                                }
                            }
                        });
                        app.grouping_handle = Some(handle);
                    }
                    Command::SpawnIncrementalGrouping {
                        backend,
                        model,
                        reasoning_effort,
                        summaries,
                        diff_hash,
                        head_commit,
                        file_hashes,
                        delta,
                    } => {
                        let tx2 = tx.clone();
                        let handle = tokio::spawn(async move {
                            match crate::grouper::llm::request_incremental_grouping(
                                backend,
                                &model,
                                reasoning_effort.as_deref(),
                                &summaries,
                            )
                            .await
                            {
                                Ok(groups) => {
                                    let _ = tx2
                                        .send(Message::IncrementalGroupingComplete(
                                            groups,
                                            delta,
                                            file_hashes,
                                            diff_hash,
                                            head_commit,
                                        ))
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx2
                                        .send(Message::GroupingFailed(e.to_string(), diff_hash))
                                        .await;
                                }
                            }
                        });
                        app.grouping_handle = Some(handle);
                    }
                    Command::SpawnReviewBatch(_) => {
                        spawn_review_batch(cmd, &tx, &mut app);
                    }
                    Command::SpawnReviewSection { .. } => {
                        // Individual spawn — handled within SpawnReviewBatch above
                        // This variant exists for type completeness but won't be sent alone in Phase 1
                    }
                    Command::CancelReview(hash) => {
                        let keys: Vec<_> = app
                            .review_handles
                            .keys()
                            .filter(|(h, _)| *h == hash)
                            .cloned()
                            .collect();
                        for key in keys {
                            if let Some(handle) = app.review_handles.remove(&key) {
                                handle.abort();
                            }
                        }
                    }

                    Command::SpawnFullContextDiff {
                        request_id,
                        git_diff_args,
                    } => {
                        let tx2 = tx.clone();

                        tokio::spawn(async move {
                            let output = tokio::process::Command::new("git")
                                .args(&git_diff_args)
                                .output()
                                .await;

                            let result = match output {
                                Ok(output) if output.status.success() => {
                                    let raw = String::from_utf8_lossy(&output.stdout);

                                    let untracked =
                                        crate::diff::untracked::discover_untracked_files_async()
                                            .await;

                                    let (data, _) =
                                        crate::diff::parse_with_untracked_paths(&raw, &untracked);

                                    Ok(data)
                                }
                                Ok(output) => {
                                    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
                                }
                                Err(error) => Err(error.to_string()),
                            };

                            let _ = tx2
                                .send(Message::FullContextLoaded { request_id, result })
                                .await;
                        });
                    }

                    Command::Quit => break,
                }
            }
        } else {
            break; // channel closed
        }
    }

    // 9. Cleanup: remove PID file and restore terminal
    signal::remove_pid_file();
    ratatui::restore();

    Ok(())
}
