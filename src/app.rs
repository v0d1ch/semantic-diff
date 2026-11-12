use crate::diff::DiffData;
use crate::grouper::llm::LlmBackend;
use crate::grouper::{GroupingStatus, SemanticGroup};
use crate::highlight::HighlightCache;
use crate::preview::mermaid::{ImageSupport, MermaidCache};
use crate::review::{GroupReview, ReviewCache, ReviewSection, ReviewSource, SectionState};
use crate::theme::Theme;
use crate::ui::file_tree::TreeNodeId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use tokio::sync::mpsc;
use tui_tree_widget::TreeState;

/// Hunk-level filter: maps file path → set of hunk indices to show.
/// An empty set means show all hunks for that file.
pub type HunkFilter = HashMap<String, HashSet<usize>>;

/// Input mode for the application.
#[derive(Debug, Clone, PartialEq)]
pub enum InputMode {
    Normal,
    Search,
    DiffSearch,
    Help,
    Settings,
}

/// Which panel currently has keyboard focus.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusedPanel {
    FileTree,
    DiffView,
}

/// Messages processed by the TEA update loop.
#[derive(Debug)]
pub enum Message {
    KeyPress(KeyEvent),
    Resize(u16, u16),
    RefreshSignal,
    DebouncedRefresh,
    DiffParsed(DiffData, String), // parsed data + raw diff for cache hashing
    GroupingComplete(Vec<SemanticGroup>, u64), // groups + diff_hash for cache saving
    GroupingFailed(String),
    IncrementalGroupingComplete(
        Vec<SemanticGroup>,
        crate::grouper::DiffDelta,
        HashMap<String, u64>,
        u64,    // diff_hash
        String, // head_commit
    ),
    /// A mermaid diagram finished rendering — triggers UI refresh.
    MermaidReady,
    ReviewSectionReady(u64, ReviewSection, Result<String, String>),
    FullContextLoaded {request_id: u64, result: Result<DiffData, String>},
}


/// Commands returned by update() for the main loop to execute.
#[allow(dead_code)]
pub enum Command {
    SpawnDiffParse { git_diff_args: Vec<String> },
    SpawnGrouping {
        backend: LlmBackend,
        model: String,
        summaries: String,
        diff_hash: u64,
        head_commit: Option<String>,
        file_hashes: HashMap<String, u64>,
    },
    SpawnIncrementalGrouping {
        backend: LlmBackend,
        model: String,
        summaries: String,
        diff_hash: u64,
        head_commit: String,
        file_hashes: HashMap<String, u64>,
        delta: crate::grouper::DiffDelta,
    },
    SpawnReviewSection {
        backend: crate::grouper::llm::LlmBackend,
        model: String,
        section: ReviewSection,
        prompt: String,
        group_content_hash: u64,
    },
    SpawnReviewBatch(Vec<Command>),
    CancelReview(u64),
    SpawnFullContextDiff {
      git_diff_args: Vec<String>,
      request_id: u64,

    },
    Quit,
}

/// Identifies a collapsible node in the diff tree.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum NodeId {
    File(usize),
    Hunk(usize, usize),
}

/// UI state for navigation and collapse tracking.
pub struct UiState {
    pub selected_index: usize,
    pub scroll_offset: u16,
    pub collapsed: HashSet<NodeId>,
    /// Terminal viewport height, updated each frame.
    pub viewport_height: u16,
    /// Width of the diff view panel (Cell for interior mutability during render).
    pub diff_view_width: Cell<u16>,
    /// Scroll offset for preview mode (line-based, not item-based).
    pub preview_scroll: usize,
    pub full_file_context: bool,
}

/// An item in the flattened visible list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleItem {
    FileHeader { file_idx: usize },
    HunkHeader { file_idx: usize, hunk_idx: usize },
    DiffLine { file_idx: usize, hunk_idx: usize, line_idx: usize },
}

impl VisibleItem {
    /// Document order, with each header preceding its children.
    fn position(&self) -> (usize, usize, usize) {
        match *self {
            Self::FileHeader { file_idx } => (file_idx, 0, 0),
            Self::HunkHeader { file_idx, hunk_idx } => (file_idx, hunk_idx + 1, 0),
            Self::DiffLine { file_idx, hunk_idx, line_idx } =>
                (file_idx, hunk_idx + 1, line_idx + 1),
        }
    }
}

#[derive(Default)]
struct ItemCache {
    identity: (usize, usize, bool),
    filter: Option<String>,
    tree_filter: Option<HunkFilter>,
    collapsed: HashSet<NodeId>,
    items: Option<Rc<Vec<VisibleItem>>>,
}

struct SearchCache {
    items: Rc<Vec<VisibleItem>>,
    query: String,
    matches: Rc<Vec<VisibleItem>>,
}

/// A code location to preserve when switching diff views.
struct DiffAnchor {
    file_path: String,
    source_line: Option<usize>,
    target_line: Option<usize>,
    screen_row: usize,
}

/// The main application state (TEA Model).
pub struct App {
    visible_cache: RefCell<ItemCache>,
    expanded_cache: RefCell<ItemCache>,
    search_cache: RefCell<Option<SearchCache>>,
    pub(crate) tree_cache: RefCell<crate::ui::file_tree::TreeCache>,
    pub(crate) preview_cache: RefCell<crate::preview::PreviewCache>,
    pub diff_data: DiffData,
    pub full_context_diff: Option<DiffData>,
    pub full_context_loading: bool,
    pub full_context_request_id: u64,
    pub ui_state: UiState,
    pub highlight_cache: HighlightCache,
    #[allow(dead_code)]
    pub should_quit: bool,
    /// Channel sender for spawning debounce timers that send DebouncedRefresh.
    pub event_tx: Option<mpsc::Sender<Message>>,
    /// Handle to the current debounce timer task, if any.
    pub debounce_handle: Option<tokio::task::JoinHandle<()>>,
    /// Current input mode, including filename and diff-text search.
    pub input_mode: InputMode,
    /// Current search query being typed.
    pub search_query: String,
    /// The confirmed filter pattern (set on Enter in search mode).
    pub active_filter: Option<String>,
    /// Confirmed literal, case-sensitive search in diff line contents.
    pub diff_search: Option<String>,
    /// Semantic groups from LLM, if available. None = ungrouped.
    pub semantic_groups: Option<Vec<SemanticGroup>>,
    /// Lifecycle state of the current grouping request.
    pub grouping_status: GroupingStatus,
    /// Handle to the in-flight grouping task, for cancellation (ROB-05).
    pub grouping_handle: Option<tokio::task::JoinHandle<()>>,
    /// Which LLM backend is available (Claude preferred, Copilot fallback), if any.
    pub llm_backend: Option<LlmBackend>,
    /// Model string resolved for the active backend.
    pub llm_model: String,
    /// Which panel currently has keyboard focus.
    pub focused_panel: FocusedPanel,
    /// Persistent tree state for tui-tree-widget (RefCell for interior mutability in render).
    pub tree_state: RefCell<TreeState<TreeNodeId>>,
    /// When a group is selected in the sidebar, filter the diff view to those (file, hunk) pairs.
    /// Key = file path (stripped), Value = set of hunk indices (empty = all hunks).
    pub tree_filter: Option<HunkFilter>,
    /// Active theme (colors + syntect theme name), derived from config at startup.
    pub theme: Theme,
    /// HEAD commit hash when current groups were computed. Used for incremental grouping.
    pub previous_head: Option<String>,
    /// Per-file content hashes from the last grouping. Used to detect what changed.
    pub previous_file_hashes: HashMap<String, u64>,
    /// Git diff arguments from the CLI, used for refreshes.
    pub git_diff_args: Vec<String>,
    /// Whether markdown preview mode is active (toggled with "p").
    pub preview_mode: bool,
    /// Whether the terminal supports inline image rendering.
    pub image_support: ImageSupport,
    /// Mermaid diagram cache (only used when image_support == Supported).
    pub mermaid_cache: Option<MermaidCache>,
    pub review_cache: ReviewCache,
    pub review_handles: std::collections::HashMap<(u64, ReviewSection), tokio::task::JoinHandle<()>>,
    pub active_review_group: Option<u64>,
    pub review_scroll: usize,
    pub review_source: ReviewSource,
    pub sidebar_visible: bool,
}

impl App {
    /// Create a new App with parsed diff data, user config, and git diff arguments.
    pub fn new(diff_data: DiffData, config: &crate::config::Config, git_diff_args: Vec<String>) -> Self {
        let theme = Theme::from_mode(config.theme_mode);
        let highlight_cache = HighlightCache::new(&diff_data, theme.syntect_theme);
        let image_support = crate::preview::mermaid::detect_image_support();
        let mermaid_cache = match &image_support {
            ImageSupport::Supported(_) => Some(MermaidCache::new()),
            _ => None,
        };
        Self {
            visible_cache: RefCell::new(ItemCache::default()),
            expanded_cache: RefCell::new(ItemCache::default()),
            search_cache: RefCell::new(None),
            tree_cache: RefCell::new(crate::ui::file_tree::TreeCache::default()),
            preview_cache: RefCell::new(crate::preview::PreviewCache::default()),
            diff_data,
            full_context_diff: None,
            full_context_loading: false,
            full_context_request_id: 0,
            ui_state: UiState {
                selected_index: 0,
                scroll_offset: 0,
                collapsed: HashSet::new(),
                viewport_height: 24, // will be updated on first draw
                diff_view_width: Cell::new(80),
                preview_scroll: 0,
                full_file_context: false,
            },
            highlight_cache,
            should_quit: false,
            event_tx: None,
            debounce_handle: None,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            active_filter: None,
            diff_search: None,
            semantic_groups: None,
            grouping_status: GroupingStatus::Idle,
            grouping_handle: None,
            llm_backend: config.detect_backend(),
            llm_model: config
                .detect_backend()
                .map(|b| config.model_for_backend(b).to_string())
                .unwrap_or_default(),
            focused_panel: FocusedPanel::DiffView,
            tree_state: RefCell::new(TreeState::default()),
            tree_filter: None,
            theme,
            previous_head: None,
            previous_file_hashes: HashMap::new(),
            git_diff_args,
            preview_mode: false,
            image_support,
            mermaid_cache,
            review_cache: ReviewCache::new(),
            review_handles: std::collections::HashMap::new(),
            active_review_group: None,
            review_scroll: 0,
            review_source: crate::review::detect_review_skill(),
            sidebar_visible: true,
        }
    }

    /// Call after directly mutating diff contents outside `update` (for example
    /// in library clients). Normal messages invalidate these caches automatically.
    pub fn invalidate_derived_data(&self) {
        *self.visible_cache.borrow_mut() = ItemCache::default();
        *self.expanded_cache.borrow_mut() = ItemCache::default();
        *self.search_cache.borrow_mut() = None;
        *self.tree_cache.borrow_mut() = Default::default();
        *self.preview_cache.borrow_mut() = Default::default();
    }

    /// TEA update: dispatch message to handler, return optional command.
    pub fn update(&mut self, msg: Message) -> Option<Command> {
        match msg {
            Message::KeyPress(key) => self.handle_key(key),
            Message::Resize(_w, h) => {
                self.ui_state.viewport_height = h.saturating_sub(1);
                None
            }
            Message::RefreshSignal => {
                // Cancel any existing debounce timer
                if let Some(handle) = self.debounce_handle.take() {
                    handle.abort();
                }
                // Spawn a new debounce timer: 500ms delay before refresh
                if let Some(tx) = &self.event_tx {
                    let tx = tx.clone();
                    self.debounce_handle = Some(tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let _ = tx.send(Message::DebouncedRefresh).await;
                    }));
                }
                None
            }
            Message::DebouncedRefresh => {
                self.debounce_handle = None;
                Some(Command::SpawnDiffParse {
                    git_diff_args: self.git_diff_args.clone(),
                })
            }
            Message::DiffParsed(new_data, raw_diff) => {
                self.apply_new_diff_data(new_data);
                let hash = crate::cache::diff_hash(&raw_diff);
                let current_head = crate::cache::get_head_commit();

                // Check exact diff hash cache first (handles identical re-triggers)
                if let Some(cached) = crate::cache::load(hash) {
                    let mut groups = cached;
                    crate::grouper::normalize_hunk_indices(&mut groups, &self.diff_data);
                    self.semantic_groups = Some(groups);
                    self.grouping_status = GroupingStatus::Done;
                    self.grouping_handle = None;
                    // Update incremental state
                    if let Some(ref head) = current_head {
                        self.previous_head = Some(head.clone());
                    }
                    self.previous_file_hashes =
                        crate::grouper::compute_all_file_hashes(&self.diff_data);
                    return None;
                }

                // Try incremental path: same HEAD + have previous groups
                let can_incremental = current_head.is_some()
                    && self.previous_head.as_ref() == current_head.as_ref()
                    && self.semantic_groups.is_some()
                    && !self.previous_file_hashes.is_empty();

                if can_incremental {
                    let new_hashes = crate::grouper::compute_all_file_hashes(&self.diff_data);
                    let delta =
                        crate::grouper::compute_diff_delta(&new_hashes, &self.previous_file_hashes);

                    if !delta.has_changes() {
                        // Nothing changed — keep existing groups
                        self.grouping_status = GroupingStatus::Done;
                        return None;
                    }

                    if delta.is_only_removals() {
                        // Only files removed — prune groups locally, no LLM needed
                        let mut groups = self.semantic_groups.clone().unwrap_or_default();
                        crate::grouper::remove_files_from_groups(&mut groups, &delta.removed_files);
                        crate::grouper::normalize_hunk_indices(&mut groups, &self.diff_data);
                        self.semantic_groups = Some(groups);
                        self.grouping_status = GroupingStatus::Done;
                        self.previous_file_hashes = new_hashes.clone();
                        // Save updated cache
                        if let Some(ref head) = current_head {
                            crate::cache::save_with_state(
                                hash,
                                self.semantic_groups.as_ref().unwrap(),
                                Some(head),
                                &new_hashes,
                            );
                        }
                        return None;
                    }

                    // New or modified files — spawn incremental LLM grouping
                    if let Some(backend) = self.llm_backend {
                        if let Some(handle) = self.grouping_handle.take() {
                            handle.abort();
                        }
                        self.grouping_status = GroupingStatus::Loading;
                        let existing = self.semantic_groups.as_ref().unwrap();
                        let summaries = crate::grouper::incremental_hunk_summaries(
                            &self.diff_data,
                            &delta,
                            existing,
                        );
                        tracing::info!(
                            new = delta.new_files.len(),
                            modified = delta.modified_files.len(),
                            removed = delta.removed_files.len(),
                            unchanged = delta.unchanged_files.len(),
                            "Incremental grouping"
                        );
                        return Some(Command::SpawnIncrementalGrouping {
                            backend,
                            model: self.llm_model.clone(),
                            summaries,
                            diff_hash: hash,
                            head_commit: current_head.unwrap(),
                            file_hashes: new_hashes,
                            delta,
                        });
                    }
                }

                // Fallback: full re-group
                if let Some(backend) = self.llm_backend {
                    // Cancel in-flight grouping (ROB-05)
                    if let Some(handle) = self.grouping_handle.take() {
                        handle.abort();
                    }
                    self.grouping_status = GroupingStatus::Loading;
                    let summaries = crate::grouper::hunk_summaries(&self.diff_data);
                    let file_hashes = crate::grouper::compute_all_file_hashes(&self.diff_data);
                    Some(Command::SpawnGrouping {
                        backend,
                        model: self.llm_model.clone(),
                        summaries,
                        diff_hash: hash,
                        head_commit: current_head,
                        file_hashes,
                    })
                } else {
                    self.grouping_status = GroupingStatus::Idle;
                    None
                }
            }
            Message::GroupingComplete(groups, diff_hash) => {
                let mut groups = groups;
                crate::grouper::normalize_hunk_indices(&mut groups, &self.diff_data);
                // Update incremental state for next refresh
                let current_head = crate::cache::get_head_commit();
                let file_hashes = crate::grouper::compute_all_file_hashes(&self.diff_data);
                // Save with full incremental state
                crate::cache::save_with_state(
                    diff_hash,
                    &groups,
                    current_head.as_deref(),
                    &file_hashes,
                );
                self.previous_head = current_head;
                self.previous_file_hashes = file_hashes;
                self.semantic_groups = Some(groups);
                self.grouping_status = GroupingStatus::Done;
                self.grouping_handle = None;
                // Reset tree state since structure changed from flat→grouped
                let mut ts = self.tree_state.borrow_mut();
                *ts = TreeState::default();
                ts.select_first();
                drop(ts);
                // Clear any stale tree filter from the flat view
                self.tree_filter = None;
                // Spawn reviews for all groups in parallel
                self.spawn_all_reviews()
            }
            Message::IncrementalGroupingComplete(new_assignments, delta, file_hashes, diff_hash, head_commit) => {
                let existing = self.semantic_groups.as_ref().cloned().unwrap_or_default();
                let mut merged =
                    crate::grouper::merge_groups(&existing, &new_assignments, &delta);
                crate::grouper::normalize_hunk_indices(&mut merged, &self.diff_data);
                // Save merged groups to cache with incremental state
                crate::cache::save_with_state(
                    diff_hash,
                    &merged,
                    Some(&head_commit),
                    &file_hashes,
                );
                self.semantic_groups = Some(merged);
                self.grouping_status = GroupingStatus::Done;
                self.grouping_handle = None;
                self.previous_file_hashes = file_hashes;
                self.previous_head = Some(head_commit);
                // Reset tree state since structure changed
                let mut ts = self.tree_state.borrow_mut();
                *ts = TreeState::default();
                ts.select_first();
                drop(ts);
                self.tree_filter = None;
                // Spawn reviews for all groups in parallel
                self.spawn_all_reviews()
            }
            Message::GroupingFailed(err) => {
                tracing::warn!("Grouping failed: {}", err);
                self.grouping_status = GroupingStatus::Error(err);
                self.grouping_handle = None;
                None // Continue showing ungrouped — graceful degradation (ROB-06)
            }
            Message::MermaidReady => None, // just triggers UI refresh
            Message::ReviewSectionReady(hash, section, result) => {
                self.review_handles.remove(&(hash, section));
                if let Some(review) = self.review_cache.get_mut(&hash) {
                    match result {
                        Ok(content) => {
                            if section == ReviewSection::How && content.trim() == "SKIP" {
                                review.sections.insert(section, SectionState::Skipped);
                            } else {
                                review.sections.insert(section, SectionState::Ready(content));
                            }
                        }
                        Err(msg) => {
                            review.sections.insert(section, SectionState::Error(msg));
                        }
                    }
                    let all_complete = review.sections.values().all(|s| s.is_complete());
                    if all_complete {
                        let review_clone = review.clone();
                        crate::review::save_review_to_disk(&review_clone);
                    }
                }
                None
            }

            Message::FullContextLoaded {request_id, result} => {
                if request_id != self.full_context_request_id {
                       return None;
                }
                self.full_context_loading = false;
            
                match result {
                    Ok(data) => {
                        tracing::debug!(
                            files = data.files.len(),
                            "Full-file context loaded"
                        );
            
                        // Capture now so navigation during loading is preserved too.
                        let anchor = self.capture_diff_anchor();
                        self.full_context_diff = Some(data);
                        self.invalidate_derived_data();
                        
                        if self.ui_state.full_file_context {
                            self.restore_diff_anchor(anchor);
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "Could not load full-file context");
                        self.full_context_diff = None;
                        self.ui_state.full_file_context = false;
                    }
                }
            
                None
            }
        }
    }

    /// Apply new diff data while preserving scroll position and collapse state.
    fn apply_new_diff_data(&mut self, new_data: DiffData) {
        if self.full_context_active() {
            self.reset_diff_navigation();
        }
        self.invalidate_full_context();

        // 1. Record collapsed state by file path (not index)
        let mut collapsed_files: HashSet<String> = HashSet::new();
        let mut collapsed_hunks: HashSet<(String, usize)> = HashSet::new();

        for node in &self.ui_state.collapsed {
            match node {
                NodeId::File(fi) => {
                    if let Some(file) = self.diff_data.files.get(*fi) {
                        collapsed_files.insert(file.target_file.clone());
                    }
                }
                NodeId::Hunk(fi, hi) => {
                    if let Some(file) = self.diff_data.files.get(*fi) {
                        collapsed_hunks.insert((file.target_file.clone(), *hi));
                    }
                }
            }
        }

        // 2. Record current selected file path for position preservation
        let selected_path = self.selected_file_path();

        // 3. Replace diff data and rebuild highlight cache
        self.diff_data = new_data;
        self.invalidate_derived_data();
        self.highlight_cache = HighlightCache::new(&self.diff_data, self.theme.syntect_theme);

        // 4. Rebuild collapsed set with new indices
        self.ui_state.collapsed.clear();
        for (fi, file) in self.diff_data.files.iter().enumerate() {
            if collapsed_files.contains(&file.target_file) {
                self.ui_state.collapsed.insert(NodeId::File(fi));
            }
            for (hi, _) in file.hunks.iter().enumerate() {
                if collapsed_hunks.contains(&(file.target_file.clone(), hi)) {
                    self.ui_state.collapsed.insert(NodeId::Hunk(fi, hi));
                }
            }
        }

        // 5. Restore selected position by file path, or clamp
        if let Some(path) = selected_path {
            let items = self.visible_items();
            let restored = items.iter().position(|item| {
                if let VisibleItem::FileHeader { file_idx } = item {
                    self.diff_data.files[*file_idx].target_file == path
                } else {
                    false
                }
            });
            if let Some(idx) = restored {
                self.ui_state.selected_index = idx;
            } else {
                self.ui_state.selected_index = self
                    .ui_state
                    .selected_index
                    .min(items.len().saturating_sub(1));
            }
        } else {
            let items_len = self.visible_items().len();
            self.ui_state.selected_index = self
                .ui_state
                .selected_index
                .min(items_len.saturating_sub(1));
        }

        self.adjust_scroll();
    }

    /// Get the file path of the currently selected item (for position preservation).
    fn selected_file_path(&self) -> Option<String> {
        let items = self.visible_items();
        let item = items.get(self.ui_state.selected_index)?;
        let fi = match item {
            VisibleItem::FileHeader { file_idx } => *file_idx,
            VisibleItem::HunkHeader { file_idx, .. } => *file_idx,
            VisibleItem::DiffLine { file_idx, .. } => *file_idx,
        };
        self.displayed_diff().files.get(fi).map(|f| f.target_file.clone())
    }

    /// Handle a key press event, branching on input mode.
    fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        match self.input_mode {
            InputMode::Normal => self.handle_key_normal(key),
            InputMode::Search => self.handle_key_search(key),
            InputMode::DiffSearch => self.handle_key_diff_search(key),
            InputMode::Help => {
                // Any key closes help
                self.input_mode = InputMode::Normal;
                None
            }
            InputMode::Settings => self.handle_key_settings(key),
        }
    }

    /// Handle keys while the Settings overlay is open.
    fn handle_key_settings(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('d') => {
                self.toggle_theme();
                None
            }
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                None
            }
            _ => None,
        }
    }

    /// Toggle between dark and light theme, rebuilding the highlight cache.
    pub fn toggle_theme(&mut self) {
        let new_theme = if self.theme.syntect_theme.contains("dark") {
            crate::theme::Theme::light()
        } else {
            crate::theme::Theme::dark()
        };
        self.theme = new_theme;
        self.highlight_cache = HighlightCache::new(&self.diff_data, self.theme.syntect_theme);
    }

    /// Handle keys in Normal mode.
    fn handle_key_normal(&mut self, key: KeyEvent) -> Option<Command> {
        // Global keys that work regardless of focused panel
        match key.code {
            KeyCode::Char('q') => return Some(Command::Quit),
            KeyCode::Char('?') => {
                self.input_mode = InputMode::Help;
                return None;
            }
            KeyCode::Char(',') => {
                self.input_mode = InputMode::Settings;
                return None;
            }
            KeyCode::Tab => {
                if self.sidebar_visible {
                  self.focused_panel = match self.focused_panel {
                      FocusedPanel::FileTree => FocusedPanel::DiffView,
                      FocusedPanel::DiffView => FocusedPanel::FileTree,
                  };
                }
                return None;
            }
            KeyCode::Esc => {
                if self.diff_search.take().is_some() {
                    return None;
                }
                if self.tree_filter.is_some() || self.active_filter.is_some() {
                    self.tree_filter = None;
                    self.active_filter = None;
                    self.ui_state.selected_index = 0;
                    self.adjust_scroll();
                    return None;
                } else {
                    return Some(Command::Quit);
                }
            }
            KeyCode::Char('/') => {
                self.input_mode = match self.focused_panel {
                    FocusedPanel::FileTree => InputMode::Search,
                    FocusedPanel::DiffView => {
                        // Content search operates on the raw diff, including when
                        // starting from a Markdown preview or a group review.
                        self.preview_mode = false;
                        self.active_review_group = None;
                        InputMode::DiffSearch
                    }
                };
                self.search_query.clear();
                return None;
            }
            KeyCode::Char('p') => {
                if crate::ui::preview_view::is_current_file_markdown(self) {
                    self.preview_mode = !self.preview_mode;
                    if self.preview_mode {
                        self.ui_state.preview_scroll = 0;
                    }
                }
                return None;
            }
            KeyCode::Char('b') => {
                self.sidebar_visible = !self.sidebar_visible;
            
                if !self.sidebar_visible {
                    self.focused_panel = FocusedPanel::DiffView;
                }
            
                return None;
            }
            _ => {}
        }

        // Route to panel-specific handler
        match self.focused_panel {
            FocusedPanel::FileTree => self.handle_key_tree(key),
            FocusedPanel::DiffView => self.handle_key_diff(key),
        }
    }

    /// Handle keys when the file tree sidebar is focused.
    fn handle_key_tree(&mut self, key: KeyEvent) -> Option<Command> {
        let mut ts = self.tree_state.borrow_mut();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                ts.key_down();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                ts.key_up();
            }
            KeyCode::Left => {
                ts.key_left();
            }
            KeyCode::Right => {
                ts.key_right();
            }
            KeyCode::Enter => {
                ts.toggle_selected();
            }
            KeyCode::Char('g') => {
                ts.select_first();
            }
            KeyCode::Char('G') => {
                ts.select_last();
            }
            _ => return None,
        }

        // After any navigation, sync the diff view to the selected tree node
        let selected = ts.selected().to_vec();
        drop(ts); // release borrow before mutating self
        self.apply_tree_selection(&selected)
    }

    /// Update the diff view filter based on the currently selected tree node.
    fn apply_tree_selection(&mut self, selected: &[TreeNodeId]) -> Option<Command> {
        match selected.last() {
            Some(TreeNodeId::Directory(group_idx, path)) => {
                self.select_tree_directory(path, *group_idx);
                None
            }
            Some(TreeNodeId::File(group_idx, path)) => {
                self.select_tree_file(path, *group_idx);
                // Clear review pane display, but let in-flight reviews finish
                // in the background so results get cached.
                self.active_review_group = None;
                self.review_scroll = 0;
                None
            }
            Some(TreeNodeId::Group(gi)) => {
                self.select_tree_group(*gi);
                // Set this group as active for the review pane display.
                // Reviews are pre-spawned for all groups when grouping completes,
                // so by the time the user navigates here the review is likely
                // already cached or in-flight.
                if let Some(groups) = &self.semantic_groups {
                    if let Some(group) = groups.get(*gi) {
                        let hash = crate::review::group_content_hash(group);
                        if self.active_review_group == Some(hash) {
                            return None; // already showing
                        }
                        self.active_review_group = Some(hash);
                        self.review_scroll = 0;

                        // If review isn't in memory yet, try disk cache
                        if self.review_cache.get(&hash).is_none() {
                            if let Some(cached) = crate::review::load_review_from_disk(hash, &self.review_source) {
                                self.review_cache.insert(cached);
                            }
                        }
                        // If still not cached (and not in-flight), spawn on demand as fallback
                        if self.review_cache.get(&hash).is_none() {
                            return self.spawn_all_reviews();
                        }
                    }
                }
                None
            }
            None => None,
        }
    }

    /// Spawn reviews for ALL groups. Checks disk cache first, only spawns LLM
    /// calls for groups without cached reviews. Returns a SpawnReviewBatch with
    /// all needed sections, or None if everything is already cached.
    pub fn spawn_all_reviews(&mut self) -> Option<Command> {
        let backend = self.llm_backend?;
        let groups = self.semantic_groups.as_ref()?.clone();
        let mut all_cmds: Vec<Command> = Vec::new();

        for group in &groups {
            let hash = crate::review::group_content_hash(group);

            // Already in memory cache?
            if self.review_cache.get(&hash).is_some() {
                continue;
            }

            // Check disk cache
            if let Some(cached) = crate::review::load_review_from_disk(hash, &self.review_source) {
                self.review_cache.insert(cached);
                continue;
            }

            // Cache miss — prepare LLM calls for this group
            let mut sections_map = std::collections::HashMap::new();
            for s in ReviewSection::all() {
                sections_map.insert(s, SectionState::Loading);
            }
            self.review_cache.insert(GroupReview {
                content_hash: hash,
                sections: sections_map,
                source: self.review_source.clone(),
            });

            let model = self.llm_model.clone();
            let review_source = self.review_source.clone();
            for &section in &ReviewSection::all() {
                let prompt = crate::review::llm::build_review_prompt(
                    section, group, &self.diff_data, &review_source,
                );
                all_cmds.push(Command::SpawnReviewSection {
                    backend,
                    model: model.clone(),
                    section,
                    prompt,
                    group_content_hash: hash,
                });
            }
        }

        if all_cmds.is_empty() {
            None
        } else {
            Some(Command::SpawnReviewBatch(all_cmds))
        }
    }

    fn full_context_git_args(&self) -> Vec<String> {
        let mut args = self.git_diff_args.clone();
    
        let insert_at = args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len());
    
        args.insert(insert_at, String::from("--unified=999999"));
    
        args
    }

    /// Handle keys when the diff view is focused (original behavior).
    fn handle_key_diff(&mut self, key: KeyEvent) -> Option<Command> {
        // Review pane scroll and controls take priority when a review is active
        if self.active_review_group.is_some() {
            return self.handle_key_review(key);
        }

        // In preview mode, redirect navigation keys to preview scroll
        if self.preview_mode {
            return self.handle_key_preview(key);
        }

        let items_len = self.visible_items().len();
        if items_len == 0 {
            return None;
        }

        match key.code {
            // Jump to next/previous search match
            KeyCode::Char('n') => {
                if self.diff_search.is_some() {
                    self.jump_to_diff_match(true, false);
                } else {
                    self.jump_to_match(true);
                }
                None
            }
            KeyCode::Char('N') => {
                if self.diff_search.is_some() {
                    self.jump_to_diff_match(false, false);
                } else {
                    self.jump_to_match(false);
                }
                None
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1, items_len);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1, items_len);
                None
            }
            KeyCode::Char('g') => {
                self.ui_state.selected_index = 0;
                self.adjust_scroll();
                None
            }
            KeyCode::Char('G') => {
                self.ui_state.selected_index = items_len.saturating_sub(1);
                self.adjust_scroll();
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half_page = (self.ui_state.viewport_height / 2) as usize;
                self.move_selection(half_page as isize, items_len);
                None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half_page = (self.ui_state.viewport_height / 2) as usize;
                self.move_selection(-(half_page as isize), items_len);
                None
            }

            // Collapse/Expand
            KeyCode::Enter => {
                self.toggle_collapse();
                None
            }

            KeyCode::Char('f') => {
                let enable = !self.ui_state.full_file_context;
                let was_active = self.full_context_active();
                let anchor = if was_active {
                    self.capture_diff_anchor()
                } else {
                    None
                };
            
                self.invalidate_full_context();
            
                if enable {
                    self.ui_state.full_file_context = true;
                    self.full_context_loading = true;
            
                    Some(Command::SpawnFullContextDiff {
                        request_id: self.full_context_request_id,
                        git_diff_args: self.full_context_git_args(),
                    })
                } else {
                    if was_active {
                        self.restore_diff_anchor(anchor);
                    }
                    None
                }
            }

            _ => None,
        }
    }

    /// Filter the diff view to show only the hunks for the selected file within its group.
    /// `group_idx` identifies which group the file was selected from (None = flat/ungrouped).
    fn select_tree_file(&mut self, path: &str, group_idx: Option<usize>) {
        let filter = self.hunk_filter_for_file(path, group_idx);
        // Always apply the filter (don't toggle — that's what group headers are for)
        self.tree_filter = Some(filter);
        // Rebuild visible items and scroll to the selected file's header
        let items = self.visible_items();
        let target_idx = items.iter().position(|item| {
            if let VisibleItem::FileHeader { file_idx } = item {
                self.displayed_diff().files[*file_idx]
                    .target_file
                    .trim_start_matches("b/")
                    == path
            } else {
                false
            }
        });
        self.ui_state.selected_index = target_idx.unwrap_or(0);
        // Pin scroll so the file header is at the top of the viewport
        self.ui_state.scroll_offset = self.ui_state.selected_index as u16;
    }

    /// Filter descendants of a directory, preserving its group's hunk restrictions.
    fn select_tree_directory(&mut self, directory: &str, group_idx: Option<usize>) {
        let mut filter = match group_idx {
            Some(gi) => self.hunk_filter_for_group(gi),
            None => {
                let mut all_files = HunkFilter::new();
                for file in &self.diff_data.files {
                    let path = file.target_file.trim_start_matches("b/").to_string();
                    all_files.insert(path, HashSet::new());
                }
                all_files
            }
        };

        let prefix = format!("{directory}/");
        filter.retain(|path, _| path.starts_with(&prefix));
        self.tree_filter = Some(filter);
        self.ui_state.selected_index = 0;
        self.ui_state.scroll_offset = 0;
        self.active_review_group = None;
        self.review_scroll = 0;
    }

    /// Filter the diff view to all changes in the selected group.
    fn select_tree_group(&mut self, group_idx: usize) {
        let filter = self.hunk_filter_for_group(group_idx);
        if filter.is_empty() {
            return;
        }
        self.tree_filter = Some(filter);
        self.ui_state.selected_index = 0;
        self.ui_state.scroll_offset = 0;
    }

    /// Build a HunkFilter for a single file's hunks within a specific group.
    /// Only shows the hunks relevant to that group, not the entire group's files.
    fn hunk_filter_for_file(&self, path: &str, group_idx: Option<usize>) -> HunkFilter {
        if let Some(groups) = &self.semantic_groups {
            if let Some(gi) = group_idx {
                if gi >= groups.len() {
                    // "Other" group — extract only this file's ungrouped hunks
                    return self.hunk_filter_for_file_in_other(path);
                }
                if let Some(group) = groups.get(gi) {
                    if let Some(filter) = self.hunk_filter_for_file_in_group(path, group) {
                        return filter;
                    }
                }
            }
            // Fallback (no group_idx or file not found in specified group):
            // search all groups for the first match
            for group in groups.iter() {
                if let Some(filter) = self.hunk_filter_for_file_in_group(path, group) {
                    return filter;
                }
            }
            return self.hunk_filter_for_file_in_other(path);
        }
        // No semantic groups — show all hunks for this file
        let mut filter = HunkFilter::new();
        filter.insert(path.to_string(), HashSet::new());
        filter
    }

    /// Build a single-file HunkFilter from a specific group's changes.
    fn hunk_filter_for_file_in_group(
        &self,
        path: &str,
        group: &crate::grouper::SemanticGroup,
    ) -> Option<HunkFilter> {
        for change in &group.changes() {
            if let Some(diff_path) = self.resolve_diff_path(&change.file) {
                if diff_path == path {
                    let mut filter = HunkFilter::new();
                    let hunk_set: HashSet<usize> = change.hunks.iter().copied().collect();
                    filter.insert(diff_path, hunk_set);
                    return Some(filter);
                }
            }
        }
        None
    }

    /// Build a single-file HunkFilter from the "Other" (ungrouped) hunks.
    fn hunk_filter_for_file_in_other(&self, path: &str) -> HunkFilter {
        let other = self.hunk_filter_for_other();
        let mut filter = HunkFilter::new();
        if let Some(hunk_set) = other.get(path) {
            filter.insert(path.to_string(), hunk_set.clone());
        } else {
            filter.insert(path.to_string(), HashSet::new());
        }
        filter
    }

    /// Build a HunkFilter for group at `group_idx`.
    fn hunk_filter_for_group(&self, group_idx: usize) -> HunkFilter {
        if let Some(groups) = &self.semantic_groups {
            if let Some(group) = groups.get(group_idx) {
                let mut filter = HunkFilter::new();
                for change in &group.changes() {
                    // Resolve to actual diff path
                    if let Some(diff_path) = self.resolve_diff_path(&change.file) {
                        let hunk_set: HashSet<usize> = change.hunks.iter().copied().collect();
                        filter
                            .entry(diff_path)
                            .or_default()
                            .extend(hunk_set.iter());
                    }
                }
                return filter;
            }
            // group_idx beyond actual groups = "Other" group
            if group_idx >= groups.len() {
                return self.hunk_filter_for_other();
            }
        }
        HunkFilter::new()
    }

    /// Build a HunkFilter for the "Other" group (ungrouped hunks).
    fn hunk_filter_for_other(&self) -> HunkFilter {
        let groups = match &self.semantic_groups {
            Some(g) => g,
            None => return HunkFilter::new(),
        };

        // Collect all grouped (file, hunk) pairs
        let mut grouped: HashMap<String, HashSet<usize>> = HashMap::new();
        for group in groups {
            for change in &group.changes() {
                if let Some(dp) = self.resolve_diff_path(&change.file) {
                    grouped.entry(dp).or_default().extend(change.hunks.iter());
                }
            }
        }

        // For each diff file, include hunks NOT covered by any group
        let mut filter = HunkFilter::new();
        for file in &self.diff_data.files {
            let dp = file.target_file.trim_start_matches("b/").to_string();
            if let Some(grouped_hunks) = grouped.get(&dp) {
                // If grouped_hunks is empty, all hunks are claimed
                if grouped_hunks.is_empty() {
                    continue;
                }
                let ungrouped: HashSet<usize> = (0..file.hunks.len())
                    .filter(|hi| !grouped_hunks.contains(hi))
                    .collect();
                if !ungrouped.is_empty() {
                    filter.insert(dp, ungrouped);
                }
            } else {
                // File not in any group — all hunks are "other"
                filter.insert(dp, HashSet::new());
            }
        }
        filter
    }

    /// Resolve a group file path to the actual diff file path (stripped of b/ prefix).
    fn resolve_diff_path(&self, group_path: &str) -> Option<String> {
        self.diff_data.files.iter().find_map(|f| {
            let dp = f.target_file.trim_start_matches("b/");
            if dp == group_path || dp.ends_with(group_path) {
                Some(dp.to_string())
            } else {
                None
            }
        })
    }

    /// Handle keys in Search mode.
    fn handle_key_search(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.search_query.clear();
                self.active_filter = None;
                None
            }
            KeyCode::Enter => {
                self.input_mode = InputMode::Normal;
                self.active_filter = if self.search_query.is_empty() {
                    None
                } else {
                    Some(self.search_query.clone())
                };
                self.ui_state.selected_index = 0;
                self.adjust_scroll();
                None
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                None
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                None
            }
            _ => None,
        }
    }

    fn handle_key_diff_search(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Esc => {
                // Cancel the draft without changing the previous search or filters.
                self.input_mode = InputMode::Normal;
                self.search_query.clear();
            }
            KeyCode::Enter => {
                self.input_mode = InputMode::Normal;
                self.diff_search = (!self.search_query.is_empty())
                    .then(|| self.search_query.clone());
                self.jump_to_diff_match(true, true);
            }
            KeyCode::Backspace => {
                self.search_query.pop();
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.search_query.push(c);
            }
            _ => {}
        }
        None
    }

    /// Use the draft while typing so highlights and feedback update immediately.
    pub fn diff_search_pattern(&self) -> Option<&str> {
        let pattern = if self.input_mode == InputMode::DiffSearch {
            Some(self.search_query.as_str())
        } else {
            self.diff_search.as_deref()
        };
        pattern.filter(|pattern| !pattern.is_empty())
    }

    fn is_diff_match(&self, item: &VisibleItem, pattern: &str) -> bool {
        match item {
            VisibleItem::DiffLine {
                file_idx,
                hunk_idx,
                line_idx,
            } => {
                self.displayed_diff().files[*file_idx].hunks[*hunk_idx].lines[*line_idx]
                    .content
                    .contains(pattern)
            }
            _ => false,
        }
    }

    /// Count matching lines in the current file/group scope, including collapsed lines.
    pub fn diff_search_status(&self) -> Option<String> {
        let pattern = self.diff_search_pattern()?;
        let current = self.visible_items().get(self.ui_state.selected_index).copied();
        let matches = self.diff_matches(pattern);
        if matches.is_empty() {
            return Some("No matches".to_owned());
        }
        let position = current
            .as_ref()
            .and_then(|current| matches.binary_search_by_key(&current.position(), VisibleItem::position).ok());
        Some(match position {
            Some(index) => format!("{}/{} matching lines", index + 1, matches.len()),
            None => format!("{} matching lines", matches.len()),
        })
    }

    fn diff_matches(&self, pattern: &str) -> Rc<Vec<VisibleItem>> {
        let items = self.cached_diff_items(false);
        let mut cache = self.search_cache.borrow_mut();
        if let Some(cache) = cache.as_ref() {
            if Rc::ptr_eq(&cache.items, &items) && cache.query == pattern {
                return Rc::clone(&cache.matches);
            }
        }
        let matches = Rc::new(items.iter().copied()
            .filter(|item| self.is_diff_match(item, pattern)).collect());
        *cache = Some(SearchCache {
            items,
            query: pattern.to_owned(),
            matches: Rc::clone(&matches),
        });
        matches
    }

    fn jump_to_diff_match(&mut self, forward: bool, include_current: bool) {
        let Some(pattern) = self.diff_search.as_deref() else {
            return;
        };
        let current = self.visible_items().get(self.ui_state.selected_index).copied();
        let matches = self.diff_matches(pattern);
        if matches.is_empty() {
            return;
        }
        let position = current.map(|item| item.position()).unwrap_or((0, 0, 0));
        let index = if forward {
            matches.partition_point(|item| item.position() < position
                || (!include_current && item.position() == position)) % matches.len()
        } else {
            (matches.partition_point(|item| item.position() < position
                || (include_current && item.position() == position)) + matches.len() - 1) % matches.len()
        };
        let target = matches[index];
        if let VisibleItem::DiffLine { file_idx, hunk_idx, .. } = target {
            self.ui_state.collapsed.remove(&NodeId::File(file_idx));
            self.ui_state.collapsed.remove(&NodeId::Hunk(file_idx, hunk_idx));
            if let Ok(index) = self.visible_items().binary_search_by_key(&target.position(), VisibleItem::position) {
                self.ui_state.selected_index = index;
                self.adjust_scroll();
            }
        }
    }

    /// Jump to the next or previous file header matching the active filter.
    fn jump_to_match(&mut self, forward: bool) {
        if self.active_filter.is_none() {
            return;
        }
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }

        let pattern = self.active_filter.as_ref().unwrap().to_lowercase();
        let len = items.len();
        let start = self.ui_state.selected_index;

        // Search through all items wrapping around
        for offset in 1..=len {
            let idx = if forward {
                (start + offset) % len
            } else {
                (start + len - offset) % len
            };
            if let VisibleItem::FileHeader { file_idx } = &items[idx] {
                let path = &self.displayed_diff().files[*file_idx].target_file;
                if path.to_lowercase().contains(&pattern) {
                    self.ui_state.selected_index = idx;
                    self.adjust_scroll();
                    return;
                }
            }
        }
    }

    /// Move selection by delta, clamping to valid range.
    fn move_selection(&mut self, delta: isize, items_len: usize) {
        let max_idx = items_len.saturating_sub(1);
        let new_idx = if delta > 0 {
            (self.ui_state.selected_index + delta as usize).min(max_idx)
        } else {
            self.ui_state.selected_index.saturating_sub((-delta) as usize)
        };
        self.ui_state.selected_index = new_idx;
        self.adjust_scroll();
    }

    /// Toggle collapse on the currently selected item.
    fn toggle_collapse(&mut self) {
        let items = self.visible_items();
        if let Some(item) = items.get(self.ui_state.selected_index) {
            let node_id = match item {
                VisibleItem::FileHeader { file_idx } => Some(NodeId::File(*file_idx)),
                VisibleItem::HunkHeader { file_idx, hunk_idx } => {
                    Some(NodeId::Hunk(*file_idx, *hunk_idx))
                }
                VisibleItem::DiffLine { .. } => None, // no-op on diff lines
            };

            if let Some(id) = node_id {
                if self.ui_state.collapsed.contains(&id) {
                    self.ui_state.collapsed.remove(&id);
                } else {
                    self.ui_state.collapsed.insert(id);
                }

                // Clamp selected_index after collapse/expand changes visible items
                let new_items_len = self.visible_items().len();
                if self.ui_state.selected_index >= new_items_len {
                    self.ui_state.selected_index = new_items_len.saturating_sub(1);
                }
                self.adjust_scroll();
            }
        }
    }

    /// Estimate the character width of a visible item's rendered line.
    fn item_char_width(&self, item: &VisibleItem) -> usize {
        match item {
            VisibleItem::FileHeader { file_idx } => {
                let file = &self.displayed_diff().files[*file_idx];
                let name = if file.is_rename {
                    format!(
                        "renamed: {} -> {}",
                        file.source_file.trim_start_matches("a/"),
                        file.target_file.trim_start_matches("b/")
                    )
                } else {
                    file.target_file.trim_start_matches("b/").to_string()
                };
                // " v " + name + " " + "+N" + " -N"
                3 + name.len()
                    + 1
                    + format!("+{}", file.added_count).len()
                    + format!(" -{}", file.removed_count).len()
            }
            VisibleItem::HunkHeader { file_idx, hunk_idx } => {
                let hunk = &self.displayed_diff().files[*file_idx].hunks[*hunk_idx];
                // "   v " + header
                5 + hunk.header.len()
            }
            VisibleItem::DiffLine {
                file_idx,
                hunk_idx,
                line_idx,
            } => {
                let line =
                    &self.displayed_diff().files[*file_idx].hunks[*hunk_idx].lines[*line_idx];
                // gutter (10) + prefix (2) + content
                12 + line.content.len()
            }
        }
    }

    /// Calculate the visual row count for an item given the available width.
    pub fn item_visual_rows(&self, item: &VisibleItem, width: u16) -> usize {
        if width == 0 {
            return 1;
        }
        let char_width = self.item_char_width(item);
        char_width.div_ceil(width as usize).max(1)
    }

    /// Adjust scroll offset to keep the selected item visible,
    /// accounting for line wrapping.
    fn adjust_scroll(&mut self) {
        let width = self.ui_state.diff_view_width.get();
        let viewport = self.ui_state.viewport_height as usize;
        let items = self.visible_items();
        let selected = self.ui_state.selected_index;

        if items.is_empty() || viewport == 0 {
            self.ui_state.scroll_offset = 0;
            return;
        }

        let scroll = self.ui_state.scroll_offset as usize;

        // Selected is above viewport
        if selected < scroll {
            self.ui_state.scroll_offset = selected as u16;
            return;
        }

        // Check if selected fits within viewport from current scroll
        let mut rows = 0usize;
        for (i, item) in items.iter().enumerate().take(selected + 1).skip(scroll) {
            rows += self.item_visual_rows(item, width);
            if rows > viewport && i < selected {
                break;
            }
        }

        if rows <= viewport {
            return;
        }

        // Selected is below viewport — find scroll that shows it at bottom
        let selected_height = self.item_visual_rows(&items[selected], width);
        if selected_height >= viewport {
            self.ui_state.scroll_offset = selected as u16;
            return;
        }

        let mut remaining = viewport - selected_height;
        let mut new_scroll = selected;
        for i in (0..selected).rev() {
            let h = self.item_visual_rows(&items[i], width);
            if h > remaining {
                break;
            }
            remaining -= h;
            new_scroll = i;
        }
        self.ui_state.scroll_offset = new_scroll as u16;
    }

    /// Compute the list of visible items respecting collapsed state, active filter,
    /// and hunk-level tree filter.
    /// The shared list is reused while only selection or scroll position changes.
    pub fn visible_items(&self) -> Rc<Vec<VisibleItem>> {
        self.cached_diff_items(true)
    }

    fn cached_diff_items(&self, respect_collapse: bool) -> Rc<Vec<VisibleItem>> {
        let data = self.displayed_diff();
        let identity = (data.files.as_ptr() as usize, data.files.len(), self.full_context_active());
        let cache = if respect_collapse { &self.visible_cache } else { &self.expanded_cache };
        let mut cache = cache.borrow_mut();
        let valid = cache.identity == identity
            && cache.filter == self.active_filter
            && cache.tree_filter == self.tree_filter
            && (!respect_collapse || cache.collapsed == self.ui_state.collapsed);
        if valid {
            if let Some(items) = &cache.items {
                return Rc::clone(items);
            }
        }
        let items = Rc::new(self.diff_items(respect_collapse));
        *cache = ItemCache {
            identity,
            filter: self.active_filter.clone(),
            tree_filter: self.tree_filter.clone(),
            collapsed: if respect_collapse { self.ui_state.collapsed.clone() } else { HashSet::new() },
            items: Some(Rc::clone(&items)),
        };
        items
    }

    fn diff_items(&self, respect_collapse: bool) -> Vec<VisibleItem> {
        let filter_lower = self
            .active_filter
            .as_ref()
            .map(|f| f.to_lowercase());

        let mut items = Vec::new();
        for (fi, file) in self.displayed_diff().files.iter().enumerate() {
            let file_path = file.target_file.trim_start_matches("b/");

            // If search filter is active, skip files that don't match
            if let Some(ref pattern) = filter_lower {
                if !file.target_file.to_lowercase().contains(pattern) {
                    continue;
                }
            }

            // Determine which hunks are visible based on tree filter
            let allowed_hunks: Option<&HashSet<usize>> =
                self.tree_filter.as_ref().and_then(|f| f.get(file_path));

            // If tree filter is active but this file isn't in it, skip entirely
            if self.tree_filter.is_some() && allowed_hunks.is_none() {
                continue;
            }

            items.push(VisibleItem::FileHeader { file_idx: fi });
            if !respect_collapse || !self.ui_state.collapsed.contains(&NodeId::File(fi)) {
                for (hi, hunk) in file.hunks.iter().enumerate() {
                    // If hunk filter is active and this hunk isn't in the set, skip it
                    // (empty set = show all hunks for this file)
                    
                    if !self.full_context_active() {
                        if let Some(hunk_set) = allowed_hunks {
                            if !hunk_set.is_empty() && !hunk_set.contains(&hi) {
                                continue;
                            }
                        }
                    }

                    items.push(VisibleItem::HunkHeader {
                        file_idx: fi,
                        hunk_idx: hi,
                    });
                    if !respect_collapse || !self.ui_state.collapsed.contains(&NodeId::Hunk(fi, hi)) {
                        for (li, _line) in hunk.lines.iter().enumerate() {
                            items.push(VisibleItem::DiffLine {
                                file_idx: fi,
                                hunk_idx: hi,
                                line_idx: li,
                            });
                        }
                    }
                }
            }
        }
        items
    }

    /// Handle keys when the review pane is active.
    fn handle_key_review(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.review_scroll = self.review_scroll.saturating_add(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.review_scroll = self.review_scroll.saturating_sub(1);
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.review_scroll = self.review_scroll.saturating_add(
                    (self.ui_state.viewport_height / 2) as usize,
                );
                None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.review_scroll = self.review_scroll.saturating_sub(
                    (self.ui_state.viewport_height / 2) as usize,
                );
                None
            }
            KeyCode::Char('g') => {
                self.review_scroll = 0;
                None
            }
            KeyCode::Char('G') => {
                self.review_scroll = 999; // will be clamped by renderer
                None
            }
            KeyCode::Char('R') => {
                // Force-refresh review
                if let Some(hash) = self.active_review_group {
                    // Cancel in-flight tasks for this hash
                    let keys: Vec<_> = self
                        .review_handles
                        .keys()
                        .filter(|(h, _)| *h == hash)
                        .cloned()
                        .collect();
                    for key in keys {
                        if let Some(handle) = self.review_handles.remove(&key) {
                            handle.abort();
                        }
                    }
                    // Clear caches
                    self.review_cache.remove(&hash);
                    crate::review::delete_review_from_disk(hash);
                    self.active_review_group = None;

                    // Re-trigger by re-applying current tree selection
                    let selected = self.tree_state.borrow().selected().to_vec();
                    return self.apply_tree_selection(&selected);
                }
                None
            }
            KeyCode::Esc => {
                // Clear review mode
                if let Some(old_hash) = self.active_review_group.take() {
                    let keys: Vec<_> = self
                        .review_handles
                        .keys()
                        .filter(|(h, _)| *h == old_hash)
                        .cloned()
                        .collect();
                    for key in keys {
                        if let Some(handle) = self.review_handles.remove(&key) {
                            handle.abort();
                        }
                    }
                }
                self.review_scroll = 0;
                None
            }
            _ => None,
        }
    }

    /// Handle keys in preview mode (line-based scrolling).
    fn handle_key_preview(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.ui_state.preview_scroll = self.ui_state.preview_scroll.saturating_add(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.ui_state.preview_scroll = self.ui_state.preview_scroll.saturating_sub(1);
                None
            }
            KeyCode::Char('g') => {
                self.ui_state.preview_scroll = 0;
                None
            }
            KeyCode::Char('G') => {
                self.ui_state.preview_scroll = usize::MAX; // will be clamped in render
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half_page = (self.ui_state.viewport_height / 2) as usize;
                self.ui_state.preview_scroll = self.ui_state.preview_scroll.saturating_add(half_page);
                None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half_page = (self.ui_state.viewport_height / 2) as usize;
                self.ui_state.preview_scroll = self.ui_state.preview_scroll.saturating_sub(half_page);
                None
            }
            // Collapse/expand still works in preview mode (no-op but don't block)
            KeyCode::Enter => None,
            // Search match jumping
            KeyCode::Char('n') | KeyCode::Char('N') => None,
            _ => None,
        }
    }

    /// TEA view: delegate rendering to the UI module.
    /// Returns pending images that must be flushed after terminal.draw().
    pub fn view(&self, frame: &mut ratatui::Frame) -> Vec<crate::ui::preview_view::PendingImage> {
        crate::ui::draw(self, frame)
    }


    pub fn displayed_diff(&self) -> &DiffData {
        if self.ui_state.full_file_context {
            if let Some(data) = &self.full_context_diff {
                return data;
            }
        }
    
        &self.diff_data
    }

    pub fn full_context_active(&self) -> bool {
        self.ui_state.full_file_context && self.full_context_diff.is_some()
    }

    fn capture_diff_anchor(&self) -> Option<DiffAnchor> {
        let items = self.visible_items();
        let selected = self.ui_state.selected_index;
        let item = items.get(selected)?;
        let data = self.displayed_diff();

        let (file_idx, source_line, target_line) = match *item {
            VisibleItem::FileHeader { file_idx } => {
                (file_idx, None, None)
            }

            VisibleItem::HunkHeader { file_idx, hunk_idx } => {
                let hunk = &data.files[file_idx].hunks[hunk_idx];

                (
                    file_idx,
                    (hunk.source_start > 0).then_some(hunk.source_start),
                    (hunk.target_start > 0).then_some(hunk.target_start),
                )
            }

            VisibleItem::DiffLine {
                file_idx,
                hunk_idx,
                line_idx,
            } => {
                let line = &data.files[file_idx].hunks[hunk_idx].lines[line_idx];
                (file_idx, line.source_line, line.target_line)
            }
        };

        let top = (self.ui_state.scroll_offset as usize).min(selected);
        let width = self.ui_state.diff_view_width.get();

        let screen_row = items[top..selected]
            .iter()
            .map(|item| self.item_visual_rows(item, width))
            .sum();

        Some(DiffAnchor {
            file_path: data.files[file_idx].target_file.clone(),
            source_line,
            target_line,
            screen_row,
        })
    }

    fn find_diff_anchor(&self, anchor: &DiffAnchor) -> Option<usize> {
        let data = self.displayed_diff();
        let wanted_file = data.files.iter().position(|file| {
            file.target_file == anchor.file_path
        })?;

        let items = self.visible_items();
        let mut file_header = None;

        // Store the smallest distance found and its visible-item index.
        let mut closest: Option<(usize, usize)> = None;

        for (index, item) in items.iter().enumerate() {
            match *item {
                VisibleItem::FileHeader { file_idx }
                    if file_idx == wanted_file =>
                {
                    file_header = Some(index);

                    // No line numbers means we originally selected the file header.
                    if anchor.source_line.is_none() && anchor.target_line.is_none() {
                        return Some(index);
                    }
                }

                VisibleItem::DiffLine {
                    file_idx,
                    hunk_idx,
                    line_idx,
                } if file_idx == wanted_file => {
                    let line = &data.files[file_idx].hunks[hunk_idx].lines[line_idx];

                    let source_line = line.source_line;
                    let target_line = line.target_line;

                    // Prefer new-version coordinates. Removed lines use old ones.
                    let distance = if let Some(saved) = anchor.target_line {
                        target_line.map(|number| number.abs_diff(saved))
                    } else {
                        anchor.source_line.and_then(|saved| {
                            source_line.map(|number| number.abs_diff(saved))
                        })
                    };

                    if let Some(distance) = distance {
                        if distance == 0 {
                            return Some(index);
                        }

                        if closest.map_or(true, |(best, _)| distance < best) {
                            closest = Some((distance, index));
                        }
                    }
                }

                _ => {}
            }
        }

        closest.map(|(_, index)| index).or(file_header)
    }

    fn restore_diff_anchor(&mut self, anchor: Option<DiffAnchor>) {
        // Full context can change hunk indices, so old collapse IDs are stale.
        self.ui_state.collapsed.clear();

        let items = self.visible_items();

        let selected = anchor
            .as_ref()
            .and_then(|saved| self.find_diff_anchor(saved))
            .unwrap_or(self.ui_state.selected_index)
            .min(items.len().saturating_sub(1));

        self.ui_state.selected_index = selected;

        let mut remaining_rows = anchor
            .as_ref()
            .map_or(0, |saved| saved.screen_row);

        let width = self.ui_state.diff_view_width.get();
        let mut top = selected;

        // Walk upward until we've restored the selection's screen position.
        while top > 0 {
            let rows = self.item_visual_rows(&items[top - 1], width);

            if rows > remaining_rows {
                break;
            }

            remaining_rows -= rows;
            top -= 1;
        }

        self.ui_state.scroll_offset = top.min(u16::MAX as usize) as u16;
        self.adjust_scroll();
    }

    fn reset_diff_navigation(&mut self) {
        self.ui_state.selected_index = 0;
        self.ui_state.scroll_offset = 0;
        self.ui_state.collapsed.clear();
    }

    fn invalidate_full_context(&mut self) {
        self.invalidate_derived_data();
        self.full_context_request_id += 1;
        self.full_context_diff = None;
        self.full_context_loading = false;
        self.ui_state.full_file_context = false;
    }
}

#[cfg(test)]
mod view_cache_tests {
    use super::*;
    use crate::config::Config;
    use crate::theme::ThemeMode;

    const DIFF: &str = "diff --git a/test.rs b/test.rs\n--- a/test.rs\n+++ b/test.rs\n@@ -10,2 +20,2 @@\n needle context\n-old needle\n+new needle\n";

    fn app() -> App {
        let mut config = Config::default_config();
        config.theme_mode = ThemeMode::Dark;
        let mut app = App::new(crate::diff::parse(DIFF), &config, vec![]);
        app.llm_backend = None;
        app
    }

    #[test]
    fn cursor_scroll_and_search_navigation_reuse_items_and_matches() {
        let mut app = app();
        let items = app.visible_items();
        let matches = app.diff_matches("needle");
        assert_eq!(matches.len(), 3);
        app.diff_search = Some("needle".into());
        for _ in 0..20 {
            app.jump_to_diff_match(true, false);
            assert!(Rc::ptr_eq(&items, &app.visible_items()));
            assert!(Rc::ptr_eq(&matches, &app.diff_matches("needle")));
            assert!(app.diff_search_status().unwrap().ends_with("/3 matching lines"));
        }
        app.ui_state.scroll_offset = 1;
        assert!(Rc::ptr_eq(&items, &app.visible_items()));
    }

    #[test]
    fn collapse_rebuilds_visible_items_but_keeps_search_scope() {
        let mut app = app();
        let items = app.visible_items();
        let matches = app.diff_matches("needle");
        app.ui_state.collapsed.insert(NodeId::File(0));
        assert_eq!(app.visible_items().len(), 1);
        assert!(!Rc::ptr_eq(&items, &app.visible_items()));
        assert!(Rc::ptr_eq(&matches, &app.diff_matches("needle")));
        app.diff_search = Some("needle".into());
        app.jump_to_diff_match(true, false);
        assert!(app.ui_state.collapsed.is_empty());
        assert_eq!(app.visible_items().len(), items.len());
    }

    #[test]
    fn query_and_filters_recompute_only_the_dependent_lists() {
        let mut app = app();
        let items = app.visible_items();
        let matches = app.diff_matches("needle");
        assert_eq!(app.diff_matches("old").len(), 1);
        assert!(Rc::ptr_eq(&items, &app.visible_items()));
        assert!(!Rc::ptr_eq(&matches, &app.diff_matches("old")));
        app.active_filter = Some("missing".into());
        assert!(app.visible_items().is_empty());
        assert!(app.diff_matches("needle").is_empty());
        app.active_filter = None;
        app.tree_filter = Some(HashMap::from([("elsewhere.rs".into(), HashSet::new())]));
        assert!(app.visible_items().is_empty());
        app.tree_filter = None;
        assert_eq!(app.diff_matches("needle").len(), 3);
    }

    #[test]
    fn refresh_and_full_context_results_invalidate_search_results() {
        let mut app = app();
        let items = app.visible_items();
        let matches = app.diff_matches("needle");
        app.apply_new_diff_data(crate::diff::parse(&DIFF.replace("needle", "changed")));
        assert!(!Rc::ptr_eq(&items, &app.visible_items()));
        assert!(!Rc::ptr_eq(&matches, &app.diff_matches("needle")));
        assert!(app.diff_matches("needle").is_empty());
        app.ui_state.full_file_context = true;
        app.update(Message::FullContextLoaded {
            request_id: app.full_context_request_id,
            result: Ok(crate::diff::parse(DIFF)),
        });
        assert_eq!(app.diff_matches("needle").len(), 3);
        app.invalidate_full_context();
        assert!(app.diff_matches("needle").is_empty());
        app.diff_data.files[0].hunks[0].lines[0].content = "needle".into();
        app.invalidate_derived_data();
        assert_eq!(app.diff_matches("needle").len(), 1);
    }

    #[test]
    fn tree_cache_tracks_groups_width_theme_and_diff_refresh() {
        use crate::ui::file_tree::build_tree_items;
        let mut app = app();
        let initial = build_tree_items(&app, 40);
        app.ui_state.selected_index = 2;
        app.focused_panel = FocusedPanel::FileTree;
        assert!(Rc::ptr_eq(&initial, &build_tree_items(&app, 40)));
        app.semantic_groups = Some(vec![SemanticGroup::new("Group".into(), "".into(), vec![
            crate::grouper::GroupedChange { file: "test.rs".into(), hunks: vec![0] },
        ])]);
        let grouped = build_tree_items(&app, 40);
        assert!(!Rc::ptr_eq(&initial, &grouped));
        assert_eq!(grouped[0].identifier(), &TreeNodeId::Group(0));
        assert!(Rc::ptr_eq(&grouped, &build_tree_items(&app, 40)));
        assert!(!Rc::ptr_eq(&grouped, &build_tree_items(&app, 25)));
        let narrow = build_tree_items(&app, 25);
        app.theme.tree_group_fg = ratatui::style::Color::Red;
        let themed = build_tree_items(&app, 25);
        assert!(!Rc::ptr_eq(&narrow, &themed));
        app.apply_new_diff_data(crate::diff::parse(""));
        assert!(build_tree_items(&app, 25).is_empty());
    }
}
