use clap::Parser;
use crate::theme::ThemeMode;
use std::path::Path;

/// A terminal diff viewer with AI-powered semantic grouping.
///
/// Drop-in replacement for `git diff` — all positional arguments and common
/// flags (--staged, --cached, --merge-base, -- paths) are passed through
/// to `git diff` directly.
///
/// Examples:
///   semantic-diff                        # unstaged changes (same as git diff) if on a branch it
///                                        # shows differences to the default branch (main/master)
///   semantic-diff HEAD                   # all changes vs HEAD
///   semantic-diff --staged               # staged changes only
///   semantic-diff main..feature          # two-dot range
///   semantic-diff main...feature         # three-dot (merge-base) range
///   semantic-diff HEAD~3 HEAD -- src/    # specific commits + path filter
#[derive(Parser, Debug)]
#[command(name = "semantic-diff", version, about)]
pub struct Cli {
    /// Color theme: auto, dark, or light. Auto-detects terminal background.
    /// Can also be set via SEMANTIC_DIFF_THEME env var.
    /// Use --theme=light for SSH/tmux sessions where auto-detection fails.
    #[arg(long, value_name = "MODE", env = "SEMANTIC_DIFF_THEME")]
    pub theme: Option<String>,

    /// Arguments passed through to `git diff` (commits, ranges, --staged, -- paths, etc.)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub git_args: Vec<String>,
}

impl Cli {
    pub fn theme_mode(&self) -> ThemeMode {
        match self.theme.as_deref() {
            Some("dark") => ThemeMode::Dark,
            Some("light") => ThemeMode::Light,
            _ => ThemeMode::Auto,
        }
    }
}

impl Cli {
    /// Build the full argument list for `git diff`, prepending `-M` for rename detection.
    pub fn git_diff_args(&self) -> Vec<String> {
        self.git_diff_args_in(Path::new("."))
    }

    // Keep Git lookups scoped to a directory so tests don't change the
    // process-wide working directory while other tests are running.
    fn git_diff_args_in(&self, repo: &Path) -> Vec<String> {
        let mut args = vec![
            "diff".to_owned(),
            "-M".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-color".to_owned(),
        ];

        // Explicit arguments take priority.
        if !self.git_args.is_empty() {
            args.extend(self.git_args.iter().cloned());
            return args;
        }

        let current_branch = git_output(repo, &[
            "symbolic-ref", "--quiet", "--short", "HEAD",
        ]);

        let default_ref = git_output(repo, &[
            "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD",
        ]);

        if let (Some(current), Some(default)) = (current_branch, default_ref) {
            let default_name = default.strip_prefix("refs/remotes/origin/");

            if default_name == Some(current.as_str()) {
                // On the default branch: compare working files with HEAD.
                args.push("HEAD".to_owned());
            } else {
                // On a feature branch: compare from the shared ancestor.
                args.push("--merge-base".to_owned());
                args.push(default);
            }
        }

        args
    }
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run fixture Git command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo_with_default_branch() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        // A nonstandard name proves we discover the default, not assume main.
        git(repo.path(), &["init", "--initial-branch=trunk"]);
        git(repo.path(), &[
            "-c", "user.name=Test", "-c", "user.email=test@example.com",
            "commit", "--allow-empty", "-m", "initial",
        ]);
        git(repo.path(), &["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
        git(repo.path(), &[
            "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk",
        ]);
        repo
    }

    #[test]
    fn test_no_args_on_default_branch_compares_with_head() {
        let repo = repo_with_default_branch();
        let cli = Cli { theme: None, git_args: vec![] };
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD"]
        );
    }

    #[test]
    fn test_no_args_on_feature_branch_uses_default_branch_merge_base() {
        let repo = repo_with_default_branch();
        git(repo.path(), &["checkout", "-b", "feature"]);
        let cli = Cli { theme: None, git_args: vec![] };
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--merge-base", "refs/remotes/origin/trunk"]
        );
    }

    #[test]
    fn test_no_args_without_default_branch_falls_back_to_unstaged() {
        let repo = repo_with_default_branch();
        git(repo.path(), &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        let cli = Cli { theme: None, git_args: vec![] };
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color"]
        );
    }

    #[test]
    fn test_no_args_on_detached_head_falls_back_to_unstaged() {
        let repo = repo_with_default_branch();
        git(repo.path(), &["checkout", "--detach"]);
        let cli = Cli { theme: None, git_args: vec![] };
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color"]
        );
    }

    #[test]
    fn test_no_args_outside_repository_falls_back_to_bare_diff() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli { theme: None, git_args: vec![] };
        assert_eq!(
            cli.git_diff_args_in(dir.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color"]
        );
    }

    #[test]
    fn test_explicit_args_override_feature_branch_default() {
        let repo = repo_with_default_branch();
        git(repo.path(), &["checkout", "-b", "feature"]);
        let cli = Cli { theme: None, git_args: vec!["--staged".to_owned()] };
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--staged"]
        );
    }

    #[test]
    fn test_head_arg() {
        let cli = Cli {
            theme: None,
            git_args: vec!["HEAD".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD"]);
    }

    #[test]
    fn test_staged_flag() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--staged".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "--staged"]);
    }

    #[test]
    fn test_two_dot_range() {
        let cli = Cli {
            theme: None,
            git_args: vec!["main..feature".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "main..feature"]);
    }

    #[test]
    fn test_three_dot_range() {
        let cli = Cli {
            theme: None,
            git_args: vec!["main...feature".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "main...feature"]);
    }

    #[test]
    fn test_two_refs() {
        let cli = Cli {
            theme: None,
            git_args: vec!["main".to_string(), "feature".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "main", "feature"]
        );
    }

    #[test]
    fn test_path_limiter() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "HEAD".to_string(),
                "--".to_string(),
                "src/".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD", "--", "src/"]
        );
    }

    #[test]
    fn test_cached_alias() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--cached".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "--cached"]);
    }

    // --- Stress / edge-case tests ---

    #[test]
    fn test_head_tilde_syntax() {
        let cli = Cli {
            theme: None,
            git_args: vec!["HEAD~3".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD~3"]);
    }

    #[test]
    fn test_head_caret_syntax() {
        let cli = Cli {
            theme: None,
            git_args: vec!["HEAD^".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD^"]);
    }

    #[test]
    fn test_sha_refs() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "abc1234".to_string(),
                "def5678".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "abc1234", "def5678"]
        );
    }

    #[test]
    fn test_full_sha() {
        let sha = "a".repeat(40);
        let cli = Cli {
            theme: None,
            git_args: vec![sha.clone()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", &sha]);
    }

    #[test]
    fn test_staged_with_ref() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--staged".to_string(), "HEAD~1".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--staged", "HEAD~1"]
        );
    }

    #[test]
    fn test_multiple_path_limiters() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "HEAD".to_string(),
                "--".to_string(),
                "src/".to_string(),
                "tests/".to_string(),
                "Cargo.toml".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD", "--", "src/", "tests/", "Cargo.toml"]
        );
    }

    #[test]
    fn test_two_dot_range_with_paths() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "main..feature".to_string(),
                "--".to_string(),
                "src/".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "main..feature", "--", "src/"]
        );
    }

    #[test]
    fn test_three_dot_range_with_paths() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "origin/main...HEAD".to_string(),
                "--".to_string(),
                "*.rs".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "origin/main...HEAD", "--", "*.rs"]
        );
    }

    #[test]
    fn test_merge_base_flag() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--merge-base".to_string(), "main".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--merge-base", "main"]
        );
    }

    #[test]
    fn test_no_index_flag() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "--no-index".to_string(),
                "file_a.txt".to_string(),
                "file_b.txt".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--no-index", "file_a.txt", "file_b.txt"]
        );
    }

    #[test]
    fn test_many_positional_args_stress() {
        let args: Vec<String> = (0..100).map(|i| format!("path_{i}.rs")).collect();
        let cli = Cli {
            theme: None,
            git_args: args.clone(),
        };
        let result = cli.git_diff_args();
        assert_eq!(result.len(), 104); // Four fixed arguments + 100 paths
        assert_eq!(result[0], "diff");
        assert_eq!(result[1], "-M");
        assert_eq!(result[2], "--no-ext-diff");
        assert_eq!(result[3], "--no-color");
        assert_eq!(result[4], "path_0.rs");
        assert_eq!(result[103], "path_99.rs");
    }

    #[test]
    fn test_unicode_path() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "HEAD".to_string(),
                "--".to_string(),
                "src/日本語/ファイル.rs".to_string(),
            ],
        };
        let result = cli.git_diff_args();
        assert_eq!(result[6], "src/日本語/ファイル.rs");
    }

    #[test]
    fn test_path_with_spaces() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "--".to_string(),
                "path with spaces/file.rs".to_string(),
            ],
        };
        let result = cli.git_diff_args();
        assert_eq!(result[5], "path with spaces/file.rs");
    }

    #[test]
    fn test_at_upstream_syntax() {
        let cli = Cli {
            theme: None,
            git_args: vec!["@{upstream}".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "@{upstream}"]);
    }

    #[test]
    fn test_stash_ref() {
        let cli = Cli {
            theme: None,
            git_args: vec!["stash@{0}".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "stash@{0}"]);
    }

    #[test]
    fn test_remote_tracking_branch() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "origin/main".to_string(),
                "origin/feature/my-branch".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "origin/main", "origin/feature/my-branch"]
        );
    }

    #[test]
    fn test_tag_ref() {
        let cli = Cli {
            theme: None,
            git_args: vec!["v1.0.0".to_string(), "v2.0.0".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "v1.0.0", "v2.0.0"]
        );
    }

    #[test]
    fn test_diff_filter_flag_passthrough() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--diff-filter=ACMR".to_string(), "HEAD".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--diff-filter=ACMR", "HEAD"]
        );
    }

    #[test]
    fn test_stat_flag_passthrough() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--stat".to_string(), "HEAD".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--stat", "HEAD"]
        );
    }

    #[test]
    fn test_name_only_flag_passthrough() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--name-only".to_string()],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--name-only"]
        );
    }

    #[test]
    fn test_combined_flags_and_ranges() {
        let cli = Cli {
            theme: None,
            git_args: vec![
                "--staged".to_string(),
                "--diff-filter=M".to_string(),
                "HEAD~5".to_string(),
                "--".to_string(),
                "src/".to_string(),
            ],
        };
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--staged", "--diff-filter=M", "HEAD~5", "--", "src/"]
        );
    }

    #[test]
    fn test_empty_string_arg() {
        let cli = Cli {
            theme: None,
            git_args: vec!["".to_string()],
        };
        let result = cli.git_diff_args();
        assert_eq!(result, vec!["diff", "-M", "--no-ext-diff", "--no-color", ""]);
    }

    #[test]
    fn test_double_dash_only() {
        let cli = Cli {
            theme: None,
            git_args: vec!["--".to_string()],
        };
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "--"]);
    }

    #[test]
    fn test_clap_parse_no_args() {
        // Simulate: semantic-diff (no arguments)
        let repo = repo_with_default_branch();
        let cli = Cli::try_parse_from(["semantic-diff"]).unwrap();
        assert!(cli.git_args.is_empty());
        assert_eq!(
            cli.git_diff_args_in(repo.path()),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD"]
        );
    }

    #[test]
    fn test_clap_parse_head() {
        let cli = Cli::try_parse_from(["semantic-diff", "HEAD"]).unwrap();
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD"]);
    }

    #[test]
    fn test_clap_parse_staged() {
        let cli = Cli::try_parse_from(["semantic-diff", "--staged"]).unwrap();
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "--staged"]);
    }

    #[test]
    fn test_clap_parse_cached() {
        let cli = Cli::try_parse_from(["semantic-diff", "--cached"]).unwrap();
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "--cached"]);
    }

    #[test]
    fn test_clap_parse_two_dot_range() {
        let cli = Cli::try_parse_from(["semantic-diff", "main..feature"]).unwrap();
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "main..feature"]);
    }

    #[test]
    fn test_clap_parse_three_dot_range() {
        let cli = Cli::try_parse_from(["semantic-diff", "main...feature"]).unwrap();
        assert_eq!(cli.git_diff_args(), vec!["diff", "-M", "--no-ext-diff", "--no-color", "main...feature"]);
    }

    #[test]
    fn test_clap_parse_two_refs() {
        let cli = Cli::try_parse_from(["semantic-diff", "abc123", "def456"]).unwrap();
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "abc123", "def456"]
        );
    }

    #[test]
    fn test_clap_parse_ref_with_paths() {
        let cli = Cli::try_parse_from([
            "semantic-diff",
            "HEAD~3",
            "--",
            "src/main.rs",
            "src/lib.rs",
        ])
        .unwrap();
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "HEAD~3", "--", "src/main.rs", "src/lib.rs"]
        );
    }

    #[test]
    fn test_clap_parse_complex_scenario() {
        let cli = Cli::try_parse_from([
            "semantic-diff",
            "--staged",
            "--diff-filter=ACMR",
            "HEAD~5",
            "--",
            "src/",
            "tests/",
        ])
        .unwrap();
        assert_eq!(
            cli.git_diff_args(),
            vec![
                "diff",
                "-M",
                "--no-ext-diff",
                "--no-color",
                "--staged",
                "--diff-filter=ACMR",
                "HEAD~5",
                "--",
                "src/",
                "tests/"
            ]
        );
    }

    #[test]
    fn test_clap_parse_merge_base() {
        let cli =
            Cli::try_parse_from(["semantic-diff", "--merge-base", "main"]).unwrap();
        assert_eq!(
            cli.git_diff_args(),
            vec!["diff", "-M", "--no-ext-diff", "--no-color", "--merge-base", "main"]
        );
    }

    #[test]
    fn test_clap_version_does_not_conflict() {
        // --version is handled by clap, should not be passed through
        let result = Cli::try_parse_from(["semantic-diff", "--version"]);
        // clap exits with a DisplayVersion error for --version
        assert!(result.is_err());
    }

    #[test]
    fn test_clap_help_does_not_conflict() {
        let result = Cli::try_parse_from(["semantic-diff", "--help"]);
        assert!(result.is_err());
    }
}
