use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use semantic_diff::app::{App, Command, DiffPurpose, FocusedPanel, Message};
use semantic_diff::cache;
use semantic_diff::config::Config;
use semantic_diff::diff;
use semantic_diff::grouper::llm::LlmBackend;
use semantic_diff::grouper::{self, GroupedChange, GroupingStatus, SemanticGroup};
use semantic_diff::review::{self, GroupReview, ReviewSection, ReviewSource, SectionState};
use semantic_diff::theme::ThemeMode;
use semantic_diff::ui::file_tree::TreeNodeId;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

// Cache APIs use the working directory. Serialize these tests within this binary
// and give each one its own repository; never write to the project's AI cache.
static REPO_LOCK: Mutex<()> = Mutex::new(());
struct Repo {
    previous: PathBuf,
    _directory: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl Repo {
    fn new() -> Self {
        let lock = REPO_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-qm",
                "test",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::write(
            directory.path().join("review.md"),
            "Explain the requested section.",
        )
        .unwrap();
        std::env::set_current_dir(directory.path()).unwrap();
        Self {
            previous,
            _directory: directory,
            _lock: lock,
        }
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
    }
}

fn raw(file: &str, added: &str) -> String {
    format!(
        "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-old\n+{added}\n"
    )
}

fn app(raw: &str) -> App {
    let mut config = Config::default_config();
    config.theme_mode = ThemeMode::Dark;
    let mut app = App::new(
        diff::parse(raw),
        &config,
        vec!["diff".into(), "HEAD".into()],
    );
    app.llm_backend = Some(LlmBackend::Codex);
    app.llm_model = "manual-ai-test".into();
    app.llm_reasoning_effort = None;
    app.review_source = ReviewSource::Skill {
        name: "fixture".into(),
        path: std::env::current_dir().unwrap().join("review.md"),
    };
    app
}

fn group(files: &[&str]) -> SemanticGroup {
    SemanticGroup::new(
        "Changes".into(),
        "Test group".into(),
        files
            .iter()
            .map(|file| GroupedChange {
                file: (*file).into(),
                hunks: vec![0],
            })
            .collect(),
    )
}

fn key(app: &mut App, key: char) -> Option<Command> {
    app.update(Message::KeyPress(KeyEvent::new(
        KeyCode::Char(key),
        KeyModifiers::NONE,
    )))
}

fn request(app: &mut App, raw: &str) -> Option<Command> {
    let Some(Command::SpawnDiffParse {
        git_diff_args,
        purpose,
    }) = key(app, 'a')
    else {
        panic!("a should request the latest diff");
    };
    assert_eq!(git_diff_args, app.git_diff_args);
    assert!(matches!(purpose, DiffPurpose::GroupAndReview));
    app.update(Message::DiffParsed(diff::parse(raw), raw.into(), purpose))
}

fn reviews(command: Option<Command>) -> usize {
    let Some(Command::SpawnReviewBatch(commands)) = command else {
        panic!("expected reviews after grouping");
    };
    assert!(commands
        .iter()
        .all(|command| matches!(command, Command::SpawnReviewSection { .. })));
    commands.len()
}

fn save_groups(app: &App, raw: &str, groups: &[SemanticGroup]) {
    cache::save_with_state(
        cache::diff_hash(raw),
        groups,
        cache::get_head_commit().as_deref(),
        &grouper::compute_all_file_hashes(&app.diff_data),
        &app.ai_cache_key(),
    );
}

#[test]
fn review_discovery_only_uses_local_review_md() {
    let _repo = Repo::new();
    assert!(matches!(
        review::detect_review_file(),
        ReviewSource::Skill { path, .. } if path == PathBuf::from("review.md")
    ));

    for directory in [".claude/skills", ".codex/skills", ".agents/skills"] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            PathBuf::from(directory).join("rust-review.md"),
            "Should not be loaded.",
        )
        .unwrap();
    }
    std::fs::remove_file("review.md").unwrap();
    assert!(matches!(
        review::detect_review_file(),
        ReviewSource::Missing
    ));
}

#[test]
fn unreadable_review_instructions_fail_sections_without_spawning_ai() {
    let _repo = Repo::new();
    let mut app = app(&raw("test.rs", "new"));
    let group = group(&["test.rs"]);
    let hash = app.review_content_hash(&group);
    app.semantic_groups = Some(vec![group]);
    app.review_source = ReviewSource::Skill {
        name: "missing review".into(),
        path: PathBuf::from("missing-review.md"),
    };

    assert!(app.spawn_all_reviews().is_none());
    let review = app.review_cache.get(&hash).unwrap();
    for section in ReviewSection::all() {
        assert!(matches!(
            review.sections.get(&section),
            Some(SectionState::Error(error)) if error.contains("missing-review.md")
        ));
    }

    std::fs::write("missing-review.md", "Explain the requested section.").unwrap();
    assert_eq!(reviews(app.spawn_all_reviews()), 4);
}

#[test]
fn startup_and_refresh_stay_idle_even_with_cache_then_a_reviews_cached_groups() {
    let _repo = Repo::new();
    let raw = raw("test.rs", "new");
    save_groups(&app(&raw), &raw, &[group(&["test.rs"])]);
    let mut app = app(&raw);
    assert!(app.semantic_groups.is_none());
    assert_eq!(app.grouping_status, GroupingStatus::Idle);
    assert!(app.active_review_group.is_none());
    let Some(Command::SpawnDiffParse { purpose, .. }) = app.update(Message::DebouncedRefresh)
    else {
        panic!("expected refresh");
    };
    assert!(matches!(purpose, DiffPurpose::Refresh));
    assert!(app
        .update(Message::DiffParsed(diff::parse(&raw), raw.clone(), purpose))
        .is_none());
    assert!(app.semantic_groups.is_none());
    assert_eq!(reviews(request(&mut app, &raw)), 4);
    assert_eq!(app.grouping_status, GroupingStatus::Done);
    assert!(app.active_review_group.is_some());
    // Repeated presses do not duplicate already loading reviews.
    assert!(request(&mut app, &raw).is_none());
}

#[test]
fn fresh_grouping_chains_reviews_and_retries_only_failed_sections() {
    let _repo = Repo::new();
    let raw = raw("test.rs", "new");
    let mut app = app(&raw);
    assert!(matches!(
        request(&mut app, &raw),
        Some(Command::SpawnGrouping { .. })
    ));
    assert_eq!(
        reviews(app.update(Message::GroupingComplete(
            vec![group(&["test.rs"])],
            cache::diff_hash(&raw)
        ))),
        4
    );
    let hash = app.active_review_group.unwrap();
    app.update(Message::ReviewSectionReady(
        hash,
        ReviewSection::Why,
        Err("timeout".into()),
    ));
    assert_eq!(reviews(request(&mut app, &raw)), 1);
    assert!(request(&mut app, &raw).is_none());
    // R remains an explicit force-refresh action in the review pane.
    app.focused_panel = FocusedPanel::DiffView;
    assert_eq!(reviews(key(&mut app, 'R')), 4);
    assert_eq!(app.active_review_group, Some(hash));
}

#[test]
fn incremental_cache_handles_changed_unchanged_and_removed_files() {
    let _repo = Repo::new();
    let original = raw("test.rs", "new") + &raw("other.rs", "other");
    let groups = vec![group(&["test.rs", "other.rs"])];
    let seed = app(&original);
    save_groups(&seed, &original, &groups);
    let changed = original.replace("+new", "+edited");
    let mut changed_app = app(&changed);
    let Some(Command::SpawnIncrementalGrouping {
        delta,
        file_hashes,
        diff_hash,
        head_commit,
        ..
    }) = request(&mut changed_app, &changed)
    else {
        panic!("expected incremental grouping from disk");
    };
    assert_eq!(delta.modified_files, vec!["test.rs"]);
    assert_eq!(
        reviews(changed_app.update(Message::IncrementalGroupingComplete(
            vec![group(&["test.rs"])],
            delta,
            file_hashes,
            diff_hash,
            head_commit,
        ))),
        4
    );

    // Different raw hash but identical parsed file contents takes the no-change path.
    save_groups(&seed, &original, &groups);
    let equivalent = format!("{original}\n");
    assert_eq!(reviews(request(&mut app(&equivalent), &equivalent)), 4);

    save_groups(&seed, &original, &groups);
    let removed = raw("test.rs", "new");
    let mut removed_app = app(&removed);
    assert_eq!(reviews(request(&mut removed_app, &removed)), 4);
    assert_eq!(
        removed_app.semantic_groups.as_ref().unwrap()[0]
            .changes()
            .len(),
        1
    );
}

#[test]
fn changed_refresh_clears_groups_and_ignores_old_grouping_results() {
    let _repo = Repo::new();
    let original = raw("test.rs", "new");
    let changed = raw("test.rs", "edited");
    let mut app = app(&original);
    assert!(matches!(
        request(&mut app, &original),
        Some(Command::SpawnGrouping { .. })
    ));
    assert!(app
        .update(Message::DiffParsed(
            diff::parse(&changed),
            changed.clone(),
            DiffPurpose::Refresh
        ))
        .is_none());
    assert!(app
        .update(Message::GroupingComplete(
            vec![group(&["test.rs"])],
            cache::diff_hash(&original)
        ))
        .is_none());
    assert!(app
        .update(Message::GroupingFailed(
            "old failure".into(),
            cache::diff_hash(&original)
        ))
        .is_none());
    assert!(app.semantic_groups.is_none());
    assert_eq!(app.grouping_status, GroupingStatus::Idle);
    assert!(app.active_review_group.is_none());
    assert!(matches!(
        request(&mut app, &changed),
        Some(Command::SpawnGrouping { .. })
    ));
}

#[test]
fn review_cache_reuses_results_but_invalidates_when_code_changes() {
    let _repo = Repo::new();
    let raw = raw("test.rs", "new");
    let groups = vec![group(&["test.rs"])];
    let mut app = app(&raw);
    let hash = app.review_content_hash(&groups[0]);
    save_groups(&app, &raw, &groups);
    review::save_review_to_disk(&GroupReview {
        content_hash: hash,
        sections: ReviewSection::all()
            .into_iter()
            .map(|section| (section, SectionState::Ready("cached".into())))
            .collect(),
        source: app.review_source.clone(),
    });
    assert!(request(&mut app, &raw).is_none());
    assert!(matches!(
        app.review_cache.get(&hash).unwrap().sections[&ReviewSection::Why],
        SectionState::Ready(_)
    ));
    let changed = raw.replace("+new", "+edited");
    app.update(Message::DiffParsed(
        diff::parse(&changed),
        changed,
        DiffPurpose::Refresh,
    ));
    assert_ne!(app.review_content_hash(&groups[0]), hash);
    assert!(app.semantic_groups.is_none());
    assert!(app.active_review_group.is_none());
}

#[test]
fn navigating_groups_never_starts_ai_and_missing_backend_is_reported() {
    let _repo = Repo::new();
    let raw = raw("test.rs", "new");
    let mut app = app(&raw);
    app.semantic_groups = Some(vec![group(&["test.rs"])]);
    app.tree_state
        .borrow_mut()
        .select(vec![TreeNodeId::Group(0)]);
    app.focused_panel = FocusedPanel::FileTree;
    assert!(app
        .update(Message::KeyPress(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )))
        .is_none());
    assert!(app
        .review_cache
        .get(&app.active_review_group.unwrap())
        .is_none());
    app.llm_backend = None;
    assert!(key(&mut app, 'a').is_none());
    assert!(app.review_notice.as_ref().unwrap().contains("No AI CLI"));
    app.update(Message::DiffParseFailed("git failed".into()));
    assert!(app.review_notice.as_ref().unwrap().contains("git failed"));
}
