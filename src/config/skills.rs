/// Agent skills — discover SKILL.md files and build catalog for system prompt.
use std::fs;
use std::path::{Path, PathBuf};

/// A discovered skill with metadata and instructions path.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Discover all skills from standard locations (project-level overrides user-level).
pub fn discover() -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Project-level (highest precedence)
    scan_dir(Path::new(".agents/skills"), &mut skills, &mut seen);
    scan_dir(Path::new(".claude/skills"), &mut skills, &mut seen);

    // User-level
    let home = super::home_dir();
    scan_dir(&home.join(".agents/skills"), &mut skills, &mut seen);
    scan_dir(&home.join(".claude/skills"), &mut skills, &mut seen);
    scan_dir(&home.join(".config/luma/skills"), &mut skills, &mut seen);

    skills
}

/// Build catalog XML for system prompt injection.
pub fn build_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\nThe following skills provide specialized instructions for specific tasks.\n\
         When a task matches a skill's description, use the Read tool with the listed \
         `location` URI to load the skill's instructions before proceeding. Skill \
         paths referenced inside a skill resolve against the skill's `directory`.\n\n\
         <available_skills>\n",
    );
    for s in skills {
        let dir = s
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "  <skill name=\"{}\">\n    <description>{}</description>\n    <location>artifact://skill/{}</location>\n    <directory>{}</directory>\n  </skill>\n",
            s.name, s.description, s.name, dir
        ));
    }
    out.push_str("</available_skills>\n");
    out
}

fn scan_dir(dir: &Path, skills: &mut Vec<Skill>, seen: &mut std::collections::HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        if let Some(skill) = parse_skill_md(&skill_md)
            && seen.insert(skill.name.clone())
        {
            skills.push(skill);
        }
    }
}

fn parse_skill_md(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;

    // Parse YAML frontmatter: --- ... ---
    if !content.starts_with("---") {
        return None;
    }
    let end = content[3..].find("---")?;
    let frontmatter = &content[3..3 + end];

    let mut name = String::new();
    let mut description = String::new();

    for line in frontmatter.lines() {
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().trim_matches('"').trim_matches('\'').to_owned();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().trim_matches('"').trim_matches('\'').to_owned();
        }
    }

    if name.is_empty() || description.is_empty() {
        return None;
    }

    Some(Skill {
        name,
        description,
        path: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_valid_skill() {
        let dir = std::env::temp_dir().join("luma_test_skill");
        let _ = fs::create_dir_all(&dir);
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: test-skill\ndescription: A test skill\n---\n# Instructions\nDo things.",
        )
        .unwrap();

        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.description, "A test skill");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_missing_fields() {
        let dir = std::env::temp_dir().join("luma_test_skill2");
        let _ = fs::create_dir_all(&dir);
        let md = dir.join("SKILL.md");
        fs::write(&md, "---\nname: only-name\n---\nNo description.").unwrap();

        assert!(parse_skill_md(&md).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_format() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "A test".into(),
            path: PathBuf::from("/tmp/test/SKILL.md"),
        }];
        let catalog = build_catalog(&skills);
        assert!(catalog.contains("<skill name=\"test\">"));
        assert!(catalog.contains("A test"));
        assert!(
            catalog.contains("<location>artifact://skill/test</location>"),
            "catalog must advertise artifact URI, not filesystem path:\n{catalog}"
        );
        assert!(
            !catalog.contains("/tmp/test/SKILL.md"),
            "raw filesystem path must not leak into catalog"
        );
    }

    #[test]
    fn empty_catalog() {
        assert!(build_catalog(&[]).is_empty());
    }
}
