//! Repository documentation information-architecture invariants.
//!
//! Human-authored Markdown is intentionally small, bilingual, and discoverable:
//! canonical project files live at the root, reference docs are flat under
//! `docs/`, and runtime-specific instructions stay beside their consumer.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_markdown(root: &Path, dir: &Path, paths: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read documentation directory") {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        let relative = path.strip_prefix(root).expect("path under repository root");
        let first = relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str());

        if matches!(first, Some(".git" | "target" | "vendor")) {
            continue;
        }
        if path.is_dir() {
            collect_markdown(root, &path, paths);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            paths.insert(relative.to_path_buf());
        }
    }
}

fn is_chinese(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".zh-TW.md"))
}

fn english_path(path: &Path) -> PathBuf {
    if !is_chinese(path) {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 Markdown filename");
    path.with_file_name(name.replace(".zh-TW.md", ".md"))
}

fn chinese_path(path: &Path) -> PathBuf {
    if is_chinese(path) {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 Markdown filename");
    path.with_file_name(name.replace(".md", ".zh-TW.md"))
}

fn canonical_location(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();

    match parts.as_slice() {
        [name] => matches!(
            *name,
            "README.md"
                | "README.zh-TW.md"
                | "CONTRIBUTING.md"
                | "CONTRIBUTING.zh-TW.md"
                | "CHANGELOG.md"
                | "CHANGELOG.zh-TW.md"
                | "CLAUDE.md"
                | "CLAUDE.zh-TW.md"
        ),
        ["docs", _] => true,
        [".github", "ISSUE_TEMPLATE", _] => true,
        [".github", name] => matches!(
            *name,
            "PULL_REQUEST_TEMPLATE.md" | "PULL_REQUEST_TEMPLATE.zh-TW.md"
        ),
        ["skills", _, "SKILL.md" | "SKILL.zh-TW.md"] => true,
        ["tests", "fixtures", ..] => true,
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DocumentShape {
    heading_levels: Vec<usize>,
    fence_languages: Vec<String>,
    table_rows: usize,
    release_keys: Vec<String>,
    env_keys: BTreeSet<String>,
}

fn document_shape(content: &str) -> DocumentShape {
    let mut heading_levels = Vec::new();
    let mut fence_languages = Vec::new();
    let mut table_rows = 0;
    let mut release_keys = Vec::new();
    let mut env_keys = BTreeSet::new();
    let mut in_fence = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        let fence = trimmed
            .strip_prefix("```")
            .or_else(|| trimmed.strip_prefix("~~~"));
        if let Some(language) = fence {
            fence_languages.push(language.trim().to_string());
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let level = line.bytes().take_while(|byte| *byte == b'#').count();
        if level > 0 && line.as_bytes().get(level) == Some(&b' ') {
            heading_levels.push(level);
            if level == 2 {
                let heading = &line[level + 1..];
                if let Some(key) = heading
                    .strip_prefix('[')
                    .and_then(|heading| heading.split_once(']').map(|(key, _)| key))
                    .filter(|key| {
                        *key == "Unreleased"
                            || key.chars().next().is_some_and(|c| c.is_ascii_digit())
                    })
                {
                    release_keys.push(key.to_string());
                }
            }
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            table_rows += 1;
        }
    }

    for (start, _) in content.match_indices("AGEND_") {
        let key: String = content[start..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if key.len() > "AGEND_".len() {
            env_keys.insert(key);
        }
    }

    DocumentShape {
        heading_levels,
        fence_languages,
        table_rows,
        release_keys,
        env_keys,
    }
}

#[test]
fn markdown_is_bilingual_and_follows_the_information_architecture() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut markdown = BTreeSet::new();
    collect_markdown(&root, &root, &mut markdown);

    let english_index = fs::read_to_string(root.join("docs/README.md")).expect("English index");
    let chinese_index =
        fs::read_to_string(root.join("docs/README.zh-TW.md")).expect("Chinese index");

    let mut errors = Vec::new();
    for path in &markdown {
        if !canonical_location(path) {
            errors.push(format!(
                "non-canonical Markdown location: {}",
                path.display()
            ));
        }

        let english = english_path(path);
        let chinese = chinese_path(path);
        if !markdown.contains(&english) || !markdown.contains(&chinese) {
            errors.push(format!(
                "missing bilingual sibling for {} (expected {} and {})",
                path.display(),
                english.display(),
                chinese.display()
            ));
            continue;
        }

        let content = fs::read_to_string(root.join(path)).expect("read Markdown");
        if content.trim().len() < 100 {
            errors.push(format!(
                "documentation is unexpectedly empty: {}",
                path.display()
            ));
        }
        if is_chinese(path)
            && !content
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        {
            errors.push(format!(
                "Chinese sibling has no CJK text: {}",
                path.display()
            ));
        }

        let sibling_name = if is_chinese(path) {
            english.file_name()
        } else {
            chinese.file_name()
        }
        .and_then(|name| name.to_str())
        .expect("UTF-8 sibling filename");
        if !content.contains(sibling_name) {
            errors.push(format!(
                "{} does not link to sibling {}",
                path.display(),
                sibling_name
            ));
        }

        if !is_chinese(path) {
            let chinese_content =
                fs::read_to_string(root.join(&chinese)).expect("read Chinese Markdown");
            let english_shape = document_shape(&content);
            let chinese_shape = document_shape(&chinese_content);
            if english_shape.heading_levels != chinese_shape.heading_levels {
                errors.push(format!(
                    "heading structure differs: {} vs {}",
                    path.display(),
                    chinese.display()
                ));
            }
            if english_shape.fence_languages != chinese_shape.fence_languages {
                errors.push(format!(
                    "code-fence structure differs: {} vs {}",
                    path.display(),
                    chinese.display()
                ));
            }
            if english_shape.table_rows != chinese_shape.table_rows {
                errors.push(format!(
                    "table-row count differs: {} ({}) vs {} ({})",
                    path.display(),
                    english_shape.table_rows,
                    chinese.display(),
                    chinese_shape.table_rows
                ));
            }
            if english_shape.release_keys != chinese_shape.release_keys {
                errors.push(format!(
                    "release keys differ: {} vs {}",
                    path.display(),
                    chinese.display()
                ));
            }
            if english_shape.env_keys != chinese_shape.env_keys {
                errors.push(format!(
                    "AGEND_* keys differ: {} vs {}",
                    path.display(),
                    chinese.display()
                ));
            }
        }

        if !is_chinese(path) && path.parent() == Some(Path::new("docs")) {
            let english_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("tracked Markdown path must have a UTF-8 file name");
            let chinese_name = chinese
                .file_name()
                .and_then(|name| name.to_str())
                .expect("paired Markdown path must have a UTF-8 file name");
            if english_name != "README.md" && !english_index.contains(english_name) {
                errors.push(format!("docs/README.md does not index {english_name}"));
            }
            if chinese_name != "README.zh-TW.md" && !chinese_index.contains(chinese_name) {
                errors.push(format!(
                    "docs/README.zh-TW.md does not index {chinese_name}"
                ));
            }
        }
    }

    assert!(
        errors.is_empty(),
        "documentation invariant violations:\n{}",
        errors.join("\n")
    );
}
