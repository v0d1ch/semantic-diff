//! Markdown preview rendering for .md files.
//!
//! Phase 7: Parses markdown via pulldown-cmark and renders to ratatui Text.
//! Phase 8: Extracts mermaid code blocks, renders via mmdc, displays with ratatui-image.
//! Phase 9: Graceful degradation for missing tools and terminal capabilities.

pub mod markdown;
pub mod mermaid;

use crate::theme::Theme;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

/// One active document: scrolling checks metadata but reuses both file contents
/// and parsed blocks. Width/theme changes reparse without rereading the file.
#[derive(Default)]
pub(crate) struct PreviewCache {
    source: Option<PreviewSource>,
    rendered: Option<(u16, Theme, Rc<Vec<markdown::PreviewBlock>>)>,
}

struct PreviewSource {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    text: String,
}

impl PreviewCache {
    pub(crate) fn blocks(&mut self, path: &Path, width: u16, theme: &Theme)
        -> Option<Rc<Vec<markdown::PreviewBlock>>>
    {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => { *self = Self::default(); return None; }
        };
        let modified = metadata.modified().ok();
        let unchanged = modified.is_some() && self.source.as_ref().is_some_and(|source|
            source.path == path && source.modified == modified && source.len == metadata.len());
        if !unchanged {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(_) => { *self = Self::default(); return None; }
            };
            self.source = Some(PreviewSource {
                path: path.to_owned(), modified, len: metadata.len(), text,
            });
            self.rendered = None;
        }
        if let Some((cached_width, cached_theme, blocks)) = &self.rendered {
            if *cached_width == width && cached_theme == theme {
                return Some(Rc::clone(blocks));
            }
        }
        let blocks = Rc::new(markdown::parse_markdown(&self.source.as_ref()?.text, width, theme));
        self.rendered = Some((width, theme.clone(), Rc::clone(&blocks)));
        Some(blocks)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::theme::ThemeMode;

    #[test]
    fn unchanged_preview_reuses_blocks_and_file_edits_replace_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview.md");
        std::fs::write(&path, "# First").unwrap();
        let theme = Theme::from_mode(ThemeMode::Dark);
        let mut cache = PreviewCache::default();
        let first = cache.blocks(&path, 80, &theme).unwrap();
        for _ in 0..10 {
            assert!(Rc::ptr_eq(&first, &cache.blocks(&path, 80, &theme).unwrap()));
        }
        std::fs::write(&path, "# A different document").unwrap();
        let edited = cache.blocks(&path, 80, &theme).unwrap();
        assert!(!Rc::ptr_eq(&first, &edited));
        assert!(format!("{edited:?}").contains("different"));
        // Same-size edits with a changed timestamp also invalidate the source.
        std::fs::write(&path, "# B different document").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + std::time::Duration::from_secs(10)).unwrap();
        assert!(!Rc::ptr_eq(&edited, &cache.blocks(&path, 80, &theme).unwrap()));
    }

    #[test]
    fn width_theme_path_and_deleted_files_do_not_reuse_stale_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview.md");
        let other = dir.path().join("other.md");
        std::fs::write(&path, "# First").unwrap();
        std::fs::write(&other, "# Other").unwrap();
        let dark = Theme::from_mode(ThemeMode::Dark);
        let light = Theme::from_mode(ThemeMode::Light);
        let mut cache = PreviewCache::default();
        let first = cache.blocks(&path, 80, &dark).unwrap();
        let narrow = cache.blocks(&path, 20, &dark).unwrap();
        assert!(!Rc::ptr_eq(&first, &narrow));
        let themed = cache.blocks(&path, 20, &light).unwrap();
        assert!(!Rc::ptr_eq(&narrow, &themed));
        let switched = cache.blocks(&other, 20, &light).unwrap();
        assert!(!Rc::ptr_eq(&themed, &switched));
        assert!(format!("{switched:?}").contains("Other"));
        std::fs::remove_file(&other).unwrap();
        assert!(cache.blocks(&other, 20, &light).is_none());
        std::fs::write(&other, "# Back").unwrap();
        assert!(format!("{:?}", cache.blocks(&other, 20, &light).unwrap()).contains("Back"));
    }
}
