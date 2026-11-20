use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

/// Render the summary bar at the bottom of the screen.
pub fn render_summary(app: &App, frame: &mut Frame, area: Rect) {
    if let Some(notice) = &app.review_notice {
        frame.render_widget(ratatui::widgets::Paragraph::new(notice.as_str()), area);
        return;
    }
    if let Some(pattern) = app.diff_search_pattern() {
        let status = app.diff_search_status().unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(
                format!(" /{pattern}  {status}"),
                Style::default().fg(app.theme.help_text_fg),
            ),
            Span::styled(
                "  n/N next/prev · Esc clear",
                Style::default().fg(app.theme.help_dismiss_fg),
            ),
        ]);
        frame.render_widget(ratatui::widgets::Paragraph::new(line), area);
        return;
    }

    if let crate::grouper::GroupingStatus::Error(error) = &app.grouping_status {
        let message = format!(" AI grouping failed: {}", grouping_error_summary(error));
        frame.render_widget(
            ratatui::widgets::Paragraph::new(message).style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }

    let total_files = app.diff_data.files.len() + app.diff_data.binary_files.len();
    let total_added: usize = app.diff_data.files.iter().map(|f| f.added_count).sum();
    let total_removed: usize = app.diff_data.files.iter().map(|f| f.removed_count).sum();

    let mut spans = vec![
        Span::styled(
            format!(" {total_files} file(s) changed  "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("+{total_added}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" -{total_removed}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ];

    if !app.diff_data.binary_files.is_empty() {
        spans.push(Span::styled(
            format!(" ({} binary)", app.diff_data.binary_files.len()),
            Style::default().fg(Color::Yellow),
        ));
    }

    // Show active filter indicator
    if let Some(ref filter) = app.active_filter {
        spans.push(Span::styled(
            format!("  [filter: {filter}]"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Show grouping status
    use crate::grouper::GroupingStatus;
    match &app.grouping_status {
        GroupingStatus::Loading => {
            spans.push(Span::styled(
                " | Grouping...",
                Style::default().fg(Color::Yellow),
            ));
        }
        GroupingStatus::Done => {
            if let Some(ref groups) = app.semantic_groups {
                spans.push(Span::styled(
                    format!(" | {} groups", groups.len()),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }
        GroupingStatus::Error(_) => {} // rendered above with the failure reason
        GroupingStatus::Idle => {}     // nothing extra
    }

    // Show preview mode indicator
    if app.preview_mode {
        spans.push(Span::styled(
            " | Preview",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(" | Raw", Style::default().fg(Color::DarkGray)));
    }

    // Right-align the shortcut hint
    spans.push(Span::styled(
        "  a AI review · ? help",
        Style::default().fg(Color::DarkGray),
    ));

    let line = Line::from(spans);
    let paragraph = ratatui::widgets::Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

/// CLI stderr can contain a startup banner, prompt, and progress before the error.
fn grouping_error_summary(error: &str) -> String {
    let reason = error
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("ERROR:"))
        .or_else(|| error.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("Unknown error; see the application log");
    reason.trim().chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn grouping_failure_displays_cli_reason_instead_of_ungrouped() {
        let mut config = crate::config::Config::default_config();
        config.theme_mode = crate::theme::ThemeMode::Dark;
        let mut app = App::new(crate::diff::parse(""), &config, vec![]);
        app.grouping_status = crate::grouper::GroupingStatus::Error(
            "codex exited with status 1:\nstartup banner\nERROR: Model is not supported\n".into(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| render_summary(&app, frame, frame.area()))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("AI grouping failed: Model is not supported"));
        assert!(!rendered.contains("Ungrouped"));
        assert_eq!(
            grouping_error_summary("LLM timed out after 60s"),
            "LLM timed out after 60s"
        );
    }
}
