use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use semantic_diff::app::{App, Command, FocusedPanel, InputMode, Message, NodeId, VisibleItem};
use semantic_diff::config::Config;
use semantic_diff::diff::{self, LineType};
use semantic_diff::theme::ThemeMode;
use std::collections::{HashMap, HashSet};

const DIFF: &str = "\
diff --git a/src/one.rs b/src/one.rs
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,3 +1,3 @@
 let needle = \"café🙂 café🙂\";
-hello old needle
+hello new needle
 quiet
@@ -20,1 +20,1 @@
 needle later
diff --git a/src/two.rs b/src/two.rs
--- a/src/two.rs
+++ b/src/two.rs
@@ -1,1 +1,1 @@
 needle elsewhere
";

fn app() -> App {
    let mut config = Config::default_config();
    config.theme_mode = ThemeMode::Dark;
    let mut app = App::new(diff::parse(DIFF), &config, vec![]);
    app.llm_backend = None;
    app.sidebar_visible = false;
    app.ui_state.viewport_height = 5;
    app.ui_state.diff_view_width.set(80);
    app
}

fn key(app: &mut App, code: KeyCode) -> Option<Command> {
    app.update(Message::KeyPress(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn type_query(app: &mut App, query: &str) {
    key(app, KeyCode::Char('/'));
    for c in query.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn search(app: &mut App, query: &str) {
    type_query(app, query);
    key(app, KeyCode::Enter);
}

fn selected(app: &App) -> (usize, usize, usize) {
    match app.visible_items()[app.ui_state.selected_index] {
        VisibleItem::DiffLine {
            file_idx,
            hunk_idx,
            line_idx,
        } => (file_idx, hunk_idx, line_idx),
        ref other => panic!("expected a selected diff line, got {other:?}"),
    }
}

fn render(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

#[test]
fn searches_all_line_types_and_wraps_in_both_directions() {
    let mut app = app();
    search(&mut app, "needle");
    assert_eq!(
        app.active_filter, None,
        "text search must not filter filenames"
    );
    assert_eq!(
        app.diff_search_status().as_deref(),
        Some("1/5 matching lines")
    );
    let expected = [(0, 0, 0), (0, 0, 1), (0, 0, 2), (0, 1, 0), (1, 0, 0)];
    for location in expected {
        assert_eq!(selected(&app), location);
        key(&mut app, KeyCode::Char('n'));
    }
    assert_eq!(selected(&app), expected[0]);
    key(&mut app, KeyCode::Char('N'));
    assert_eq!(selected(&app), expected[4]);
    assert!(app.ui_state.scroll_offset > 0);
    let (fi, hi, li) = expected[1];
    assert_eq!(
        app.diff_data.files[fi].hunks[hi].lines[li].line_type,
        LineType::Removed
    );
    assert!(text(&render(&app, 80, 6)).contains("needle elsewhere"));
}

#[test]
fn submit_includes_current_line_and_next_skips_it() {
    let mut app = app();
    search(&mut app, "hello old");
    search(&mut app, "needle");
    assert_eq!(selected(&app), (0, 0, 1));
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(selected(&app), (0, 0, 2));
}

#[test]
fn matches_expand_collapsed_parents_and_respect_file_and_hunk_filters() {
    let mut app = app();
    app.tree_filter = Some(HashMap::from([(
        "src/one.rs".to_owned(),
        HashSet::from([1]),
    )]));
    app.active_filter = Some("one".to_owned());
    app.ui_state.collapsed.insert(NodeId::File(0));
    app.ui_state.collapsed.insert(NodeId::Hunk(0, 1));
    app.ui_state.collapsed.insert(NodeId::File(1));
    search(&mut app, "needle");
    assert_eq!(selected(&app), (0, 1, 0));
    assert_eq!(
        app.diff_search_status().as_deref(),
        Some("1/1 matching lines")
    );
    assert!(!app.ui_state.collapsed.contains(&NodeId::File(0)));
    assert!(!app.ui_state.collapsed.contains(&NodeId::Hunk(0, 1)));
    assert!(app.ui_state.collapsed.contains(&NodeId::File(1)));
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(selected(&app), (0, 1, 0));
    assert!(key(&mut app, KeyCode::Esc).is_none());
    assert!(app.diff_search.is_none());
    assert!(app.tree_filter.is_some());
    assert_eq!(app.active_filter.as_deref(), Some("one"));
}

#[test]
fn filename_filter_remains_available_in_sidebar() {
    let mut app = app();
    app.sidebar_visible = true;
    app.focused_panel = FocusedPanel::FileTree;
    type_query(&mut app, "two");
    assert_eq!(app.input_mode, InputMode::Search);
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.active_filter.as_deref(), Some("two"));
    assert!(app.diff_search.is_none());
    key(&mut app, KeyCode::Tab);
    search(&mut app, "needle");
    assert_eq!(selected(&app), (1, 0, 0));
    assert_eq!(
        app.diff_search_status().as_deref(),
        Some("1/1 matching lines")
    );
}

#[test]
fn draft_cancel_empty_query_and_no_matches_preserve_navigation() {
    let mut app = app();
    search(&mut app, "needle");
    let location = selected(&app);
    type_query(&mut app, "NEEDLE");
    assert_eq!(app.diff_search_status().as_deref(), Some("No matches"));
    assert!(text(&render(&app, 100, 8)).contains("No matches"));
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.diff_search.as_deref(), Some("needle"));
    assert_eq!(selected(&app), location);
    search(&mut app, "[not a regex.*]");
    assert_eq!(app.diff_search_status().as_deref(), Some("No matches"));
    key(&mut app, KeyCode::Char('n'));
    key(&mut app, KeyCode::Char('N'));
    assert_eq!(selected(&app), location);
    search(&mut app, "");
    assert!(app.diff_search.is_none());
    assert!(app.diff_search_status().is_none());
    assert_eq!(selected(&app), location);
    type_query(&mut app, "🙂");
    key(&mut app, KeyCode::Backspace);
    assert!(app.search_query.is_empty());
}

#[test]
fn highlights_unicode_repeated_matches_and_matches_crossing_style_spans() {
    let mut app = app();
    for query in ["café🙂", "let needle", "hello new"] {
        // Draft highlights can be inspected without moving the selection.
        app.ui_state.scroll_offset = 0;
        type_query(&mut app, query);
        let buffer = render(&app, 100, 12);
        let row = (0..10)
            .find(|y| {
                (0..100)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(query)
            })
            .expect("matching content should be visible");
        let highlighted: String = (0..100)
            .filter(|x| buffer[(*x, row)].bg == app.theme.search_match_bg)
            .map(|x| buffer[(x, row)].symbol())
            .collect();
        // A wide emoji's continuation cell is blank in the terminal buffer.
        if query == "café🙂" {
            assert_eq!(highlighted.replace(' ', ""), "café🙂café🙂");
        } else {
            assert_eq!(highlighted, query);
        }
        assert_ne!(
            buffer[(0, row)].bg,
            app.theme.search_match_bg,
            "gutter must not be highlighted"
        );
        key(&mut app, KeyCode::Esc);
    }
}

#[test]
fn full_file_search_uses_loaded_context_and_updates_when_returning_to_diff() {
    let mut app = app();
    let request_id = match key(&mut app, KeyCode::Char('f')) {
        Some(Command::SpawnFullContextDiff { request_id, .. }) => request_id,
        _ => panic!("expected full-context request"),
    };
    let mut full = diff::parse(DIFF);
    full.files[0].hunks[0].lines[0].content = "only in full context".into();
    app.update(Message::FullContextLoaded {
        request_id,
        result: Ok(full),
    });
    search(&mut app, "only in full context");
    assert_eq!(selected(&app), (0, 0, 0));
    assert_eq!(
        app.diff_search_status().as_deref(),
        Some("1/1 matching lines")
    );
    assert!(text(&render(&app, 100, 12)).contains("only in full context"));
    key(&mut app, KeyCode::Char('f'));
    assert_eq!(app.diff_search_status().as_deref(), Some("No matches"));
}

#[test]
fn empty_diff_is_safe_and_search_switches_preview_and_review_to_raw() {
    let mut app = app();
    app.preview_mode = true;
    app.active_review_group = Some(1);
    type_query(&mut app, "needle");
    assert_eq!(app.input_mode, InputMode::DiffSearch);
    assert!(!app.preview_mode);
    assert!(app.active_review_group.is_none());
    key(&mut app, KeyCode::Enter);
    app.diff_data.files.clear();
    key(&mut app, KeyCode::Char('n'));
    key(&mut app, KeyCode::Char('N'));
    assert_eq!(app.diff_search_status().as_deref(), Some("No matches"));
    render(&app, 20, 3);
}
