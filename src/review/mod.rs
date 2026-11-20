pub mod clipboard;
pub mod llm;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// Identifies a review section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewSection {
    Why,
    What,
    How,
    Verdict,
}

impl ReviewSection {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Why => "WHY",
            Self::What => "WHAT",
            Self::How => "HOW",
            Self::Verdict => "VERDICT",
        }
    }

    pub fn all() -> [ReviewSection; 4] {
        [Self::Why, Self::What, Self::How, Self::Verdict]
    }
}

/// Loading state for a single review section.
#[derive(Debug, Clone)]
pub enum SectionState {
    Loading,
    Ready(String),
    Error(String),
    Skipped,
}

impl SectionState {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Error(_) | Self::Skipped)
    }
}

/// Tracks which review source was used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReviewSource {
    /// A review instruction file was discovered and is used for every section.
    Skill { name: String, path: PathBuf },
    /// No instruction file was discovered. Reviews require a file at runtime.
    #[serde(alias = "BuiltIn")]
    Missing,
}

/// Aggregate review state for one semantic group.
#[derive(Debug, Clone)]
pub struct GroupReview {
    pub content_hash: u64,
    pub sections: HashMap<ReviewSection, SectionState>,
    pub source: ReviewSource,
}

impl GroupReview {
    /// Export the whole review, independently of viewport size or scroll position.
    pub fn markdown(&self, label: &str) -> Option<String> {
        if !self
            .sections
            .values()
            .any(|state| matches!(state, SectionState::Ready(_)))
        {
            return None;
        }
        let mut output = format!("# Review: {label}\n");
        for section in ReviewSection::all() {
            let text = match self.sections.get(&section) {
                Some(SectionState::Ready(text)) => text.as_str(),
                Some(SectionState::Error(_)) => "*This section failed to load.*",
                Some(SectionState::Skipped) => continue,
                _ => "*This section is still loading.*",
            };
            output.push_str(&format!("\n## {}\n\n{text}\n", section.label()));
        }
        Some(output)
    }
}

const MAX_CACHED_REVIEWS: usize = 20;

/// Cache of reviews keyed by group content hash. Bounded LRU.
pub struct ReviewCache {
    entries: HashMap<u64, GroupReview>,
    order: VecDeque<u64>,
}

impl Default for ReviewCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, hash: &u64) -> Option<&GroupReview> {
        self.entries.get(hash)
    }

    pub fn get_mut(&mut self, hash: &u64) -> Option<&mut GroupReview> {
        if self.entries.contains_key(hash) {
            // Promote to MRU
            self.order.retain(|h| h != hash);
            self.order.push_back(*hash);
        }
        self.entries.get_mut(hash)
    }

    pub fn insert(&mut self, review: GroupReview) {
        let hash = review.content_hash;
        if self.entries.contains_key(&hash) {
            // Move to back (most recent)
            self.order.retain(|h| *h != hash);
        } else if self.entries.len() >= MAX_CACHED_REVIEWS {
            // Evict oldest
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.order.push_back(hash);
        self.entries.insert(hash, review);
    }

    pub fn remove(&mut self, hash: &u64) {
        self.entries.remove(hash);
        self.order.retain(|h| h != hash);
    }
}

/// Compute a stable hash for a group's review identity.
/// Includes label, file paths, and hunk indices so the cache invalidates
/// when group membership or hunk assignment changes.
pub fn group_content_hash(group: &crate::grouper::SemanticGroup) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    group.label.hash(&mut hasher);
    let mut changes = group.changes();
    changes.sort_by(|a, b| a.file.cmp(&b.file));
    for c in &changes {
        c.file.hash(&mut hasher);
        c.hunks.hash(&mut hasher);
    }
    hasher.finish()
}

/// Discover only review.md in the current working directory.
pub fn detect_review_file() -> ReviewSource {
    let path = PathBuf::from("review.md");
    if path.is_file() {
        ReviewSource::Skill {
            name: "review.md".to_owned(),
            path,
        }
    } else {
        ReviewSource::Missing
    }
}

/// Disk-cached review entry. Only stores completed section content.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedReview {
    pub content_hash: u64,
    pub source: ReviewSource,
    pub sections: HashMap<String, CachedSection>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CachedSection {
    Ready(String),
    Skipped,
}

fn review_cache_dir() -> PathBuf {
    let git_dir = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| PathBuf::from(".git"));
    git_dir.join("semantic-diff-cache").join("reviews")
}

fn review_cache_path(content_hash: u64) -> PathBuf {
    review_cache_dir().join(format!("{}.json", content_hash))
}

/// Save a completed review to disk. Only saves if all sections succeeded
/// (Ready or Skipped). Reviews with errors are not cached so they can be retried.
pub fn save_review_to_disk(review: &GroupReview) {
    // Don't cache if any section errored — those should be retried
    let has_errors = review
        .sections
        .values()
        .any(|s| matches!(s, SectionState::Error(_)));
    if has_errors {
        return;
    }
    let mut sections = HashMap::new();
    for (sec, state) in &review.sections {
        match state {
            SectionState::Ready(content) => {
                sections.insert(
                    sec.label().to_string(),
                    CachedSection::Ready(content.clone()),
                );
            }
            SectionState::Skipped => {
                sections.insert(sec.label().to_string(), CachedSection::Skipped);
            }
            _ => return, // Loading state shouldn't happen here, but bail if it does
        }
    }
    if sections.len() < 4 {
        return;
    }
    let cached = CachedReview {
        content_hash: review.content_hash,
        source: review.source.clone(),
        sections,
    };
    let dir = review_cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = review_cache_path(review.content_hash);
    if let Ok(json) = serde_json::to_string_pretty(&cached) {
        let _ = std::fs::write(path, json);
    }
}

/// Load a review from disk cache, validating against the current review source.
pub fn load_review_from_disk(
    content_hash: u64,
    current_source: &ReviewSource,
) -> Option<GroupReview> {
    let path = review_cache_path(content_hash);
    let data = std::fs::read_to_string(path).ok()?;
    let cached: CachedReview = serde_json::from_str(&data).ok()?;

    match (&cached.source, current_source) {
        (ReviewSource::Missing, _) | (_, ReviewSource::Missing) => return None,
        (ReviewSource::Skill { path: p1, .. }, ReviewSource::Skill { path: p2, .. }) => {
            if p1 != p2 {
                return None;
            }
        }
    }

    let mut sections = HashMap::new();
    for sec in ReviewSection::all() {
        if let Some(cached_sec) = cached.sections.get(sec.label()) {
            match cached_sec {
                CachedSection::Ready(content) => {
                    sections.insert(sec, SectionState::Ready(content.clone()));
                }
                CachedSection::Skipped => {
                    sections.insert(sec, SectionState::Skipped);
                }
            }
        }
    }

    if sections.len() < 4 {
        return None;
    }

    Some(GroupReview {
        content_hash,
        sections,
        source: current_source.clone(),
    })
}

/// Delete a disk cache entry (used for force-refresh).
pub fn delete_review_from_disk(content_hash: u64) {
    let path = review_cache_path(content_hash);
    let _ = std::fs::remove_file(path);
}
