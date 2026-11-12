use crate::app::{App, FocusedPanel};
use crate::grouper::GroupingStatus;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Block;
use ratatui::Frame;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::rc::Rc;
use tui_tree_widget::{Tree, TreeItem};

/// Identifier for tree nodes.
/// Files include an optional group index to disambiguate the same file in multiple groups.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum TreeNodeId {
    Group(usize),
    File(Option<usize>, String), // (group_index, path)
    Directory(Option<usize>, String),
}

impl std::fmt::Display for TreeNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreeNodeId::Group(i) => write!(f, "group-{i}"),
            TreeNodeId::File(Some(gi), path) => write!(f, "file-{gi}-{path}"),
            TreeNodeId::File(None, path) => write!(f, "file-{path}"),
            TreeNodeId::Directory(Some(gi), path) => write!(f, "dir-{gi}-{path}"),
            TreeNodeId::Directory(None, path) => write!(f, "dir-{path}"),
        }
    }
}

struct FileEntry {
    path: String,
    is_untracked: bool,
    suffix: String,
    added: usize,
    removed: usize,
}

#[derive(Default)]
struct Directory {
    directories: BTreeMap<String, Directory>,
    files: BTreeMap<String, FileEntry>,
}

impl Directory {
    fn insert(&mut self, entry: FileEntry) {
        // Own the components so we can move `entry` into the map later.
        let mut components: Vec<String> = entry.path.split('/').map(str::to_owned).collect();
        let Some(filename) = components.pop() else {
            return;
        };

        let mut directory = self;
        for component in components {
            directory = directory.directories.entry(component).or_default();
        }
        directory.files.insert(filename, entry);
    }

    fn into_tree_items(
        self,
        group_idx: Option<usize>,
        parent_path: &str,
        sidebar_width: u16,
        depth: u16,
    ) -> Vec<TreeItem<'static, TreeNodeId>> {
        let mut items = Vec::new();

        // Emit directories first. BTreeMap iterates in key order.
        for (name, directory) in self.directories {
            let path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            let children =
                directory.into_tree_items(group_idx, &path, sidebar_width, depth.saturating_add(1));
            let label = Line::from(Span::styled(
                format!("{name}/"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            let item = TreeItem::new(TreeNodeId::Directory(group_idx, path), label, children)
                .expect("directory builder produces unique sibling identifiers");
            items.push(item);
        }

        // Existing overhead: borders(2), highlight(3), node symbol(2).
        let path_overhead = 7u16.saturating_add(depth.saturating_mul(2));
        for (filename, file) in self.files {
            let line = build_file_text(
                &filename,
                file.is_untracked,
                &file.suffix,
                file.added,
                file.removed,
                sidebar_width,
                path_overhead,
            );
            items.push(TreeItem::new_leaf(
                TreeNodeId::File(group_idx, file.path),
                line,
            ));
        }

        items
    }
}

/// Build tree items from current app state.
/// `sidebar_width` is the total pixel/char width of the sidebar area (including borders).
#[derive(Default)]
pub(crate) struct TreeCache {
    identity: (usize, usize),
    groups: Option<Vec<crate::grouper::SemanticGroup>>,
    width: u16,
    group_fg: Option<Color>,
    items: Option<Rc<Vec<TreeItem<'static, TreeNodeId>>>>,
}

pub fn build_tree_items(app: &App, sidebar_width: u16) -> Rc<Vec<TreeItem<'static, TreeNodeId>>> {
    let identity = (app.diff_data.files.as_ptr() as usize, app.diff_data.files.len());
    let mut cache = app.tree_cache.borrow_mut();
    if cache.identity == identity && cache.groups == app.semantic_groups
        && cache.width == sidebar_width && cache.group_fg == Some(app.theme.tree_group_fg)
    {
        if let Some(items) = &cache.items {
            return Rc::clone(items);
        }
    }
    let items = Rc::new(match &app.semantic_groups {
        Some(groups) => build_grouped_tree(app, groups, sidebar_width),
        None => build_flat_tree(app, sidebar_width),
    });
    *cache = TreeCache {
        identity,
        groups: app.semantic_groups.clone(),
        width: sidebar_width,
        group_fg: Some(app.theme.tree_group_fg),
        items: Some(Rc::clone(&items)),
    };
    items
}

/// Abbreviate directory components in a path to fit within `max_width` characters.
///
/// Keeps the filename intact and progressively abbreviates parent directories
/// (left-to-right) to their first character per hyphen-separated segment.
///
/// Example: `src/app/components/sales-assistant/routes.ts` → `s/a/c/s-a/routes.ts`
fn abbreviate_path(path: &str, max_width: usize) -> String {
    if path.len() <= max_width || max_width == 0 {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return path.to_string(); // just a filename, can't abbreviate
    }

    let filename = parts.last().unwrap();
    let mut dirs: Vec<String> = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Abbreviate directories left-to-right until it fits
    for i in 0..dirs.len() {
        let candidate = format!("{}/{}", dirs.join("/"), filename);
        if candidate.len() <= max_width {
            return candidate;
        }
        // Abbreviate: for hyphenated names like "sales-assistant" → "s-a",
        // for plain names like "components" → "c"
        let dir = &dirs[i];
        if dir.len() > 1 {
            let abbreviated: String = dir
                .split('-')
                .filter_map(|seg| seg.chars().next())
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("-");
            dirs[i] = abbreviated;
        }
    }

    format!("{}/{}", dirs.join("/"), filename)
}

/// Wrap a sequence of styled spans into multiple `Line`s that fit within `max_width`.
/// Uses word-level wrapping (splits on spaces). Continuation lines are indented by `indent` spaces.
fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize, indent: usize) -> Text<'static> {
    if max_width == 0 {
        return Text::from(Line::from(spans));
    }

    let total_len: usize = spans.iter().map(|s| s.content.len()).sum();
    if total_len <= max_width {
        return Text::from(Line::from(spans));
    }

    // Flatten all spans into word-level tokens preserving styles
    struct StyledWord {
        text: String,
        style: Style,
    }
    let mut words: Vec<StyledWord> = Vec::new();
    for span in &spans {
        let style = span.style;
        let content = span.content.as_ref();
        // Split into segments preserving spaces as trailing chars on words
        let mut start = 0;
        for (i, ch) in content.char_indices() {
            if ch == ' ' {
                // Include this space with the preceding word
                let end = i + 1;
                if end > start {
                    words.push(StyledWord {
                        text: content[start..end].to_string(),
                        style,
                    });
                    start = end;
                }
            }
        }
        if start < content.len() {
            words.push(StyledWord {
                text: content[start..].to_string(),
                style,
            });
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_len: usize = 0;
    let mut is_first_line = true;

    for word in words {
        let word_len = word.text.len();
        let line_max = if is_first_line {
            max_width
        } else {
            max_width.saturating_sub(indent)
        };

        if current_len + word_len <= line_max || current_len == 0 {
            // Fits on current line, or it's the first word on the line (must place it)
            current_spans.push(Span::styled(word.text, word.style));
            current_len += word_len;
        } else {
            // Wrap to next line
            // Trim trailing space from last span on current line
            if let Some(last) = current_spans.last_mut() {
                let trimmed = last.content.trim_end().to_string();
                *last = Span::styled(trimmed, last.style);
            }
            lines.push(Line::from(std::mem::take(&mut current_spans)));
            is_first_line = false;
            current_len = 0;

            // Add indent for continuation
            let indent_str = " ".repeat(indent);
            current_len += indent;
            current_spans.push(Span::raw(indent_str));

            current_spans.push(Span::styled(word.text, word.style));
            current_len += word_len;
        }
    }

    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    Text::from(lines)
}

/// Build a Line for a file entry in the tree, with optional [U] badge, path abbreviation, and stats.
fn build_file_text(
    path: &str,
    is_untracked: bool,
    suffix: &str,
    added: usize,
    removed: usize,
    sidebar_width: u16,
    path_overhead: u16,
) -> Line<'static> {
    let badge = if is_untracked { "[U] " } else { "" };
    let stats = format!("{suffix} +{added} -{removed}");
    let max_path_width = sidebar_width
        .saturating_sub(path_overhead)
        .saturating_sub(stats.len() as u16)
        .saturating_sub(badge.len() as u16) as usize;
    let display_path = abbreviate_path(path, max_path_width);

    let mut spans = Vec::new();
    if is_untracked {
        spans.push(Span::styled(
            "[U] ".to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ));
    }
    spans.push(Span::raw(format!("{display_path}{suffix} ")));
    spans.push(Span::styled(
        format!("+{added}"),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("-{removed}"),
        Style::default().fg(Color::Red),
    ));
    Line::from(spans)
}

/// Build a flat list of file items (pre-grouping or when no LLM is available).
fn build_flat_tree<'a>(app: &App, sidebar_width: u16) -> Vec<TreeItem<'a, TreeNodeId>> {
    let mut root = Directory::default();
    for file in &app.diff_data.files {
        root.insert(FileEntry {
            path: file.target_file.trim_start_matches("b/").to_string(),
            is_untracked: file.is_untracked,
            suffix: String::new(),
            added: file.added_count,
            removed: file.removed_count,
        });
    }
    root.into_tree_items(None, "", sidebar_width, 0)
}

/// Build a grouped tree from semantic groups (hunk-level).
/// Files can appear in multiple groups if their hunks are split.
fn build_grouped_tree<'a>(
    app: &App,
    groups: &[crate::grouper::SemanticGroup],
    sidebar_width: u16,
) -> Vec<TreeItem<'a, TreeNodeId>> {
    let mut all_covered: std::collections::HashMap<String, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    let mut items: Vec<TreeItem<'a, TreeNodeId>> = Vec::new();

    // Available width for group header: sidebar - borders(2) - highlight(3) - node_symbol(2)
    let group_overhead: u16 = 2 + 3 + 2;

    let files_by_path: HashMap<_, _> = app.diff_data.files.iter().enumerate()
        .map(|(index, file)| (file.target_file.trim_start_matches("b/"), index))
        .collect();
    // Compute each hunk's statistics once, even when several groups reference it.
    let hunk_counts: Vec<Vec<(usize, usize)>> = app.diff_data.files.iter().map(|file| {
        file.hunks.iter().map(|hunk| {
            hunk.lines.iter().fold((0, 0), |(added, removed), line| match line.line_type {
                crate::diff::LineType::Added => (added + 1, removed),
                crate::diff::LineType::Removed => (added, removed + 1),
                crate::diff::LineType::Context => (added, removed),
            })
        }).collect()
    }).collect();

    for (gi, group) in groups.iter().enumerate() {
        let mut root = Directory::default();
        let mut group_added: usize = 0;
        let mut group_removed: usize = 0;

        for change in &group.changes() {
            let file_index = files_by_path.get(change.file.as_str()).copied().or_else(|| {
                app.diff_data.files.iter().position(|file|
                    file.target_file.trim_start_matches("b/").ends_with(change.file.as_str()))
            });
            if let Some(file_index) = file_index {
                let file = &app.diff_data.files[file_index];
                let path = file.target_file.trim_start_matches("b/").to_string();

                // Count lines for the specific hunks in this group
                let (added, removed) = if change.hunks.is_empty() {
                    // All hunks
                    (file.added_count, file.removed_count)
                } else {
                    change.hunks.iter().fold((0usize, 0usize), |(a, r), &hi| {
                        if let Some(&(ha, hr)) = hunk_counts[file_index].get(hi) {
                            (a + ha, r + hr)
                        } else {
                            (a, r)
                        }
                    })
                };

                group_added += added;
                group_removed += removed;

                // Track covered hunks
                all_covered
                    .entry(path.clone())
                    .or_default()
                    .extend(change.hunks.iter());

                // Show hunk count if not all hunks
                let hunk_info = if change.hunks.is_empty() || change.hunks.len() == file.hunks.len()
                {
                    String::new()
                } else {
                    format!(" ({}/{} hunks)", change.hunks.len(), file.hunks.len())
                };

                root.insert(FileEntry {
                    path,
                    is_untracked: file.is_untracked,
                    suffix: hunk_info,
                    added,
                    removed,
                });
            }
        }

        let children = root.into_tree_items(Some(gi), "", sidebar_width, 1);
        let file_count = count_file_items(&children);

        if !children.is_empty() {
            let header_spans = vec![
                Span::styled(
                    format!("{} ", group.label),
                    Style::default()
                        .fg(app.theme.tree_group_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("+{group_added}"), Style::default().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(format!("-{group_removed}"), Style::default().fg(Color::Red)),
                Span::styled(
                    format!(", {file_count} files"),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            let available_width = sidebar_width.saturating_sub(group_overhead) as usize;
            let header = wrap_spans(header_spans, available_width, 2);
            if let Ok(item) = TreeItem::new(TreeNodeId::Group(gi), header, children) {
                items.push(item);
            }
        }
    }

    // Add "Other" group for hunks not in any semantic group
    let mut other_root = Directory::default();
    for file in &app.diff_data.files {
        let path = file.target_file.trim_start_matches("b/").to_string();
        let covered = all_covered.get(&path);

        let is_other = match covered {
            None => true, // file not in any group
            Some(hunk_set) => {
                // If hunk_set is empty, the LLM said "all hunks" → fully covered
                if hunk_set.is_empty() {
                    false
                } else {
                    // Check if some hunks are uncovered
                    (0..file.hunks.len()).any(|hi| !hunk_set.contains(&hi))
                }
            }
        };

        if is_other {
            other_root.insert(FileEntry {
                path,
                is_untracked: file.is_untracked,
                suffix: String::new(),
                added: file.added_count,
                removed: file.removed_count,
            });
        }
    }

    let other_children = other_root.into_tree_items(Some(groups.len()), "", sidebar_width, 1);
    let other_file_count = count_file_items(&other_children);

    if !other_children.is_empty() {
        let header = Line::from(vec![Span::styled(
            format!("Other ({other_file_count} files)"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )]);
        if let Ok(item) = TreeItem::new(TreeNodeId::Group(groups.len()), header, other_children) {
            items.push(item);
        }
    }

    items
}

fn count_file_items(items: &[TreeItem<'_, TreeNodeId>]) -> usize {
    items
        .iter()
        .map(|item| match item.identifier() {
            TreeNodeId::File(_, _) => 1,
            _ => count_file_items(item.children()),
        })
        .sum()
}

/// Render the file tree sidebar.
pub fn render_tree(app: &App, frame: &mut Frame, area: Rect) {
    let items = build_tree_items(app, area.width);

    let title = match app.grouping_status {
        GroupingStatus::Loading => " Files [grouping...] ",
        _ => " Files ",
    };

    let border_style = if app.focused_panel == FocusedPanel::FileTree {
        Style::default().fg(app.theme.tree_group_fg)
    } else {
        Style::default().fg(app.theme.gutter_fg)
    };

    let tree = match Tree::new(&items) {
        Ok(tree) => tree
            .block(Block::bordered().title(title).border_style(border_style))
            .highlight_style(
                Style::default()
                    .fg(app.theme.tree_highlight_fg)
                    .bg(app.theme.tree_highlight_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .node_closed_symbol("> ")
            .node_open_symbol("v ")
            .node_no_children_symbol("  "),
        Err(_) => return,
    };

    frame.render_stateful_widget(tree, area, &mut app.tree_state.borrow_mut());
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abbreviate_path_fits() {
        assert_eq!(abbreviate_path("src/main.rs", 30), "src/main.rs");
    }

    #[test]
    fn test_abbreviate_path_short_dirs() {
        // "src/app/components/routes.ts" = 27 chars
        // After abbreviating "src" → "s": "s/app/components/routes.ts" = 25
        // Still > 24, abbreviate "app" → "a": "s/a/components/routes.ts" = 23
        assert_eq!(
            abbreviate_path("src/app/components/routes.ts", 24),
            "s/a/components/routes.ts"
        );
    }

    #[test]
    fn test_abbreviate_path_all_dirs() {
        assert_eq!(
            abbreviate_path("src/app/components/routes.ts", 15),
            "s/a/c/routes.ts"
        );
    }

    #[test]
    fn test_abbreviate_path_hyphenated() {
        assert_eq!(
            abbreviate_path("src/app/components/sales-assistant/routes.ts", 20),
            "s/a/c/s-a/routes.ts"
        );
    }

    #[test]
    fn test_abbreviate_path_single_component() {
        assert_eq!(abbreviate_path("routes.ts", 5), "routes.ts");
    }

    #[test]
    fn test_abbreviate_path_zero_width() {
        assert_eq!(abbreviate_path("src/main.rs", 0), "src/main.rs");
    }

    #[test]
    fn test_abbreviate_path_already_short() {
        assert_eq!(abbreviate_path("a/b.rs", 10), "a/b.rs");
    }

    #[test]
    fn test_abbreviate_path_exact_fit_after_partial() {
        // "src/app/main.rs" = 15 chars
        // After abbreviating first: "s/app/main.rs" = 13 chars
        assert_eq!(abbreviate_path("src/app/main.rs", 13), "s/app/main.rs");
    }
}
