use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use semantic_diff::app::{App, Command, Message, NodeId, VisibleItem};
use semantic_diff::config::Config;
use semantic_diff::diff;
use semantic_diff::theme::ThemeMode;
use std::collections::{HashMap, HashSet};

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

// Two compact hunks merge into one full-file hunk. The insertion at line 10
// makes source and target line numbers differ for the later change.
fn position_fixture(full: bool) -> String {
    let mut patch = String::new();
    for path in ["first.rs", "second.rs"] {
        patch.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
        ));
        if full {
            patch.push_str("@@ -1,40 +1,41 @@\n");
        }
        for number in 1..=40 {
            if !full {
                match number {
                    9 => patch.push_str("@@ -9,3 +9,4 @@\n"),
                    29 => patch.push_str("@@ -29,3 +30,3 @@\n"),
                    10..=11 | 30..=31 => {}
                    _ => continue,
                }
            }
            match number {
                10 => patch.push_str("-old 10\n+new 10\n+inserted 10\n"),
                30 => patch.push_str("-old 30\n+new 30\n"),
                // Exercise visual-row preservation with a wrapped context line.
                29 => patch.push_str(&format!(" context 29 {}\n", "x".repeat(70))),
                _ => patch.push_str(&format!(" context {number}\n")),
            }
        }
    }
    patch
}

fn position_app() -> App {
    let mut config = Config::default_config();
    config.theme_mode = ThemeMode::Dark;
    let data = diff::parse(&position_fixture(false));
    assert_eq!(data.files.len(), 2);
    assert!(data.files.iter().all(|file| file.hunks.len() == 2));
    let mut app = App::new(data, &config, vec!["diff".into(), "HEAD".into()]);
    app.ui_state.diff_view_width.set(50);
    app.ui_state.viewport_height = 12;
    app
}

fn select_line(app: &mut App, path: &str, content: &str) {
    let index = app
        .visible_items()
        .iter()
        .position(|item| {
            if let VisibleItem::DiffLine {
                file_idx,
                hunk_idx,
                line_idx,
            } = *item
            {
                let file = &app.displayed_diff().files[file_idx];
                file.target_file == path && file.hunks[hunk_idx].lines[line_idx].content == content
            } else {
                false
            }
        })
        .expect("fixture contains requested line");
    app.ui_state.selected_index = index;
    app.ui_state.scroll_offset = index.saturating_sub(2) as u16;
}

fn selected_line(app: &App) -> (&str, &str) {
    let items = app.visible_items();
    let VisibleItem::DiffLine {
        file_idx,
        hunk_idx,
        line_idx,
    } = items[app.ui_state.selected_index]
    else {
        panic!("selection should be a diff line");
    };
    let file = &app.displayed_diff().files[file_idx];
    (
        &file.target_file,
        &file.hunks[hunk_idx].lines[line_idx].content,
    )
}

fn screen_row(app: &App) -> usize {
    app.visible_items()[app.ui_state.scroll_offset as usize..app.ui_state.selected_index]
        .iter()
        .map(|item| app.item_visual_rows(item, app.ui_state.diff_view_width.get()))
        .sum()
}

fn finish_request(app: &mut App, request_id: u64) {
    let data = diff::parse(&position_fixture(true));
    assert!(data.files.iter().all(|file| file.hunks.len() == 1));
    app.update(Message::FullContextLoaded {
        request_id,
        result: Ok(data),
    });
}

#[test]
fn toggling_preserves_added_removed_and_context_lines_and_screen_position() {
    for content in ["new 30", "old 30", "context 31", "inserted 10"] {
        let mut app = position_app();
        select_line(&mut app, "second.rs", content);
        let selected = app.ui_state.selected_index;
        let scroll = app.ui_state.scroll_offset;
        let row = screen_row(&app);

        let request_id = start_request(&mut app);
        assert_eq!(app.ui_state.selected_index, selected);
        assert_eq!(app.ui_state.scroll_offset, scroll);
        finish_request(&mut app, request_id);
        assert_eq!(selected_line(&app), ("second.rs", content));
        assert_eq!(screen_row(&app), row);

        assert!(press_f(&mut app).is_none());
        assert_eq!(selected_line(&app), ("second.rs", content));
        assert_eq!(screen_row(&app), row);
    }
}

#[test]
fn completion_preserves_navigation_performed_while_loading() {
    let mut app = position_app();
    select_line(&mut app, "first.rs", "new 10");
    let request_id = start_request(&mut app);
    select_line(&mut app, "second.rs", "old 30");
    let row = screen_row(&app);
    finish_request(&mut app, request_id);
    assert_eq!(selected_line(&app), ("second.rs", "old 30"));
    assert_eq!(screen_row(&app), row);
}

#[test]
fn cancellation_and_failure_leave_compact_navigation_untouched() {
    for cancel in [true, false] {
        let mut app = position_app();
        app.ui_state.collapsed.insert(NodeId::File(0));
        select_line(&mut app, "second.rs", "new 30");
        let selected = app.ui_state.selected_index;
        let scroll = app.ui_state.scroll_offset;
        let request_id = start_request(&mut app);
        if cancel {
            press_f(&mut app);
            // A canceled request must not later change the display or position.
            finish_request(&mut app, request_id);
        } else {
            app.update(Message::FullContextLoaded {
                request_id,
                result: Err("Git failed".into()),
            });
        }
        assert!(!app.ui_state.full_file_context);
        assert!(!app.full_context_loading);
        assert_eq!(app.ui_state.selected_index, selected);
        assert_eq!(app.ui_state.scroll_offset, scroll);
        assert!(app.ui_state.collapsed.contains(&NodeId::File(0)));
    }
}

#[test]
fn compact_mode_uses_nearest_line_within_the_active_hunk_filter() {
    let mut app = position_app();
    app.tree_filter = Some(HashMap::from([("second.rs".into(), HashSet::from([1]))]));
    select_line(&mut app, "second.rs", "new 30");
    let request_id = start_request(&mut app);
    finish_request(&mut app, request_id);

    // Line 10 is outside the compact filter. Returning must stay in hunk 1.
    select_line(&mut app, "second.rs", "inserted 10");
    press_f(&mut app);
    let expected = format!("context 29 {}", "x".repeat(70));
    assert_eq!(selected_line(&app), ("second.rs", expected.as_str()));
    assert_eq!(
        app.tree_filter.as_ref().unwrap()["second.rs"],
        HashSet::from([1])
    );
}

#[test]
fn selected_file_header_stays_on_the_same_file() {
    let mut app = position_app();
    let index = app
        .visible_items()
        .iter()
        .position(|item| matches!(item, VisibleItem::FileHeader { file_idx: 1 }))
        .unwrap();
    app.ui_state.selected_index = index;
    app.ui_state.scroll_offset = index as u16;
    let request_id = start_request(&mut app);
    finish_request(&mut app, request_id);
    assert!(matches!(
        app.visible_items()[app.ui_state.selected_index],
        VisibleItem::FileHeader { file_idx: 1 }
    ));
    press_f(&mut app);
    assert_eq!(app.ui_state.selected_index, index);
    assert_eq!(app.ui_state.scroll_offset as usize, index);
}

#[test]
fn hunk_header_tracks_its_location_when_full_context_merges_hunks() {
    let mut app = position_app();
    let index = app
        .visible_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                VisibleItem::HunkHeader {
                    file_idx: 1,
                    hunk_idx: 1
                }
            )
        })
        .unwrap();
    app.ui_state.selected_index = index;
    app.ui_state.scroll_offset = index as u16;
    let request_id = start_request(&mut app);
    finish_request(&mut app, request_id);
    let expected = format!("context 29 {}", "x".repeat(70));
    assert_eq!(selected_line(&app), ("second.rs", expected.as_str()));
    assert_eq!(screen_row(&app), 0);
}
