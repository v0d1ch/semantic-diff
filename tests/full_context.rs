use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use semantic_diff::app::{App, Command, Message};
use semantic_diff::config::Config;
use semantic_diff::diff;
use semantic_diff::theme::ThemeMode;

const COMPACT_DIFF: &str = "\
diff --git a/example.rs b/example.rs
--- a/example.rs
+++ b/example.rs
@@ -2 +2 @@
-old
+new
";

const FULL_DIFF: &str = "\
diff --git a/example.rs b/example.rs
--- a/example.rs
+++ b/example.rs
@@ -1,3 +1,3 @@
 before
-old
+new
 after
";

fn press_f(app: &mut App) -> Option<Command> {
    app.update(Message::KeyPress(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::NONE,
    )))
}

fn start_request(app: &mut App) -> u64 {
    match press_f(app) {
        Some(Command::SpawnFullContextDiff { request_id, .. }) => request_id,
        _ => panic!("enabling full-file context should request a diff"),
    }
}

#[test]
fn late_full_context_results_do_not_interfere_with_a_newer_request() {
    let mut config = Config::default_config();
    // Avoid querying the test runner's terminal for its background color.
    config.theme_mode = ThemeMode::Dark;
    let mut app = App::new(
        diff::parse(COMPACT_DIFF),
        &config,
        vec![String::from("diff"), String::from("HEAD")],
    );

    // Start loading, turn the view off, then request it again.
    // Commands are inspected rather than executed: no Git process or sleeps.
    let old_request = start_request(&mut app);
    assert!(press_f(&mut app).is_none());
    assert!(!app.ui_state.full_file_context);
    assert!(!app.full_context_loading);
    let current_request = start_request(&mut app);
    assert_ne!(old_request, current_request);

    // Neither an old success nor an old failure may finish the newer request.
    for result in [
        Ok(diff::parse(FULL_DIFF)),
        Err(String::from("outdated Git failure")),
    ] {
        app.update(Message::FullContextLoaded {
            request_id: old_request,
            result,
        });

        assert!(
            app.full_context_loading,
            "the newer request is still pending"
        );
        assert!(
            app.ui_state.full_file_context,
            "the user's choice must survive"
        );
        assert!(
            app.full_context_diff.is_none(),
            "old data must not be stored"
        );
        assert_eq!(app.displayed_diff().files[0].hunks[0].lines.len(), 2);
    }

    // The current result is accepted for display, leaving the AI's diff intact.
    app.update(Message::FullContextLoaded {
        request_id: current_request,
        result: Ok(diff::parse(FULL_DIFF)),
    });

    assert!(!app.full_context_loading);
    assert!(app.full_context_active());
    assert_eq!(app.displayed_diff().files[0].hunks[0].lines.len(), 4);
    assert_eq!(app.diff_data.files[0].hunks[0].lines.len(), 2);

    // Turning the feature off restores the compact view.
    assert!(press_f(&mut app).is_none());
    assert!(!app.full_context_active());
    assert_eq!(app.displayed_diff().files[0].hunks[0].lines.len(), 2);
}
