use super::{ReviewSection, ReviewSource};
use crate::diff::DiffData;
use crate::grouper::llm::{invoke_llm_text, LlmBackend};
use crate::grouper::SemanticGroup;

fn build_shared_context(group: &SemanticGroup, diff_data: &DiffData) -> String {
    let mut ctx = format!(
        "GROUP: {}\nDESCRIPTION: {}\n\nDIFF:\n",
        group.label, group.description
    );

    for change in group.changes() {
        for f in &diff_data.files {
            let path = f.target_file.trim_start_matches("b/");
            if path == change.file {
                ctx.push_str(&format!("\nFILE: {}\n", path));
                if change.hunks.is_empty() {
                    for (i, hunk) in f.hunks.iter().enumerate() {
                        let content: String = hunk
                            .lines
                            .iter()
                            .map(|l| l.content.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        ctx.push_str(&format!("HUNK {}:\n{}\n", i, content));
                    }
                } else {
                    for &hi in &change.hunks {
                        if let Some(hunk) = f.hunks.get(hi) {
                            let content: String = hunk
                                .lines
                                .iter()
                                .map(|l| l.content.as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            ctx.push_str(&format!("HUNK {}:\n{}\n", hi, content));
                        }
                    }
                }
                break;
            }
        }
    }

    ctx
}

fn build_section_prompt(
    section: ReviewSection,
    shared_context: &str,
    review_source: &ReviewSource,
) -> Result<String, String> {
    let instructions = match review_source {
        ReviewSource::Skill { path, .. } => std::fs::read_to_string(path).map_err(|error| {
            format!(
                "Could not read review instructions {}: {error}",
                path.display()
            )
        })?,
        ReviewSource::Missing => {
            return Err("No review instruction file found. Add review.md to the working directory and restart the app.".to_owned());
        }
    };
    if instructions.trim().is_empty() {
        return Err("Review instructions are empty".to_owned());
    }

    Ok(format!(
        "{instructions}\n\nSECTION: {}\n\n{shared_context}",
        section.label()
    ))
}

/// Build the full prompt for a review section.
pub fn build_review_prompt(
    section: ReviewSection,
    group: &SemanticGroup,
    diff_data: &DiffData,
    review_source: &ReviewSource,
) -> Result<String, String> {
    let shared = build_shared_context(group, diff_data);
    build_section_prompt(section, &shared, review_source)
}

/// Invoke the LLM for a single review section with a 120-second timeout.
pub async fn invoke_review_section(
    backend: LlmBackend,
    model: &str,
    reasoning_effort: Option<&str>,
    prompt: &str,
) -> Result<String, String> {
    use std::time::Duration;
    match tokio::time::timeout(
        Duration::from_secs(120),
        invoke_llm_text(backend, model, reasoning_effort, prompt),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("LLM timed out after 120s".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_source_is_an_error_for_every_section() {
        for section in ReviewSection::all() {
            let error = build_section_prompt(section, "GROUP: example", &ReviewSource::Missing)
                .unwrap_err();
            assert!(error.contains("No review instruction file found"));
        }
    }

    #[test]
    fn every_section_reads_the_supplied_path_at_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom-policy.txt");
        std::fs::write(&path, "Review allocation costs and explain tradeoffs.").unwrap();
        let source = ReviewSource::Skill {
            name: "review.md".into(),
            path: path.clone(),
        };
        for section in ReviewSection::all() {
            let prompt = build_section_prompt(section, "GROUP: example", &source).unwrap();
            assert_eq!(
                prompt,
                format!(
                    "Review allocation costs and explain tradeoffs.\n\nSECTION: {}\n\nGROUP: example",
                    section.label()
                )
            );
        }
        std::fs::write(path, "Updated instructions.").unwrap();
        let prompt =
            build_section_prompt(ReviewSection::Verdict, "GROUP: example", &source).unwrap();
        assert!(prompt.starts_with("Updated instructions.\n"));
        assert!(!prompt.contains("Review allocation costs"));
    }

    #[test]
    fn unreadable_and_empty_instructions_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        let source = ReviewSource::Skill {
            name: "review.md".into(),
            path: path.clone(),
        };
        let error = build_section_prompt(ReviewSection::Verdict, "", &source).unwrap_err();
        assert!(error.contains("Could not read review instructions"));
        assert!(error.contains(&path.display().to_string()));

        std::fs::write(path, " \n\t").unwrap();
        assert_eq!(
            build_section_prompt(ReviewSection::Verdict, "", &source).unwrap_err(),
            "Review instructions are empty"
        );
    }
}
