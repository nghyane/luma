/// Agent skills — discover SKILL.md files and build catalog for system prompt.
///
/// Interop: accepts the SKILL.md format used by Claude Code (`.claude/skills/`),
/// OpenAI Codex (`~/.codex/skills/`, including nested layouts), Kiro
/// (`.kiro/skills/`), and the community `.agents/skills/` convention.
/// Each root is walked recursively so nested layouts are picked up.
use std::fs;
use std::path::{Path, PathBuf};

/// A discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// URI prefix used in the catalog and by the Read tool.
pub const SKILL_URI_PREFIX: &str = "artifact://skill/";

const SKILL_FILE: &str = "SKILL.md";
const MAX_SCAN_DEPTH: usize = 6;

/// Discover all skills. Precedence: workspace roots first, then user-wide.
pub fn discover() -> Vec<Skill> {
    let roots = discovery_roots();
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in &roots {
        scan_recursive(root, 0, &mut skills, &mut seen);
    }
    skills
}

pub fn discovery_roots() -> Vec<PathBuf> {
    let home = super::home_dir();
    vec![
        PathBuf::from(".luma/skills"),
        PathBuf::from(".agents/skills"),
        PathBuf::from(".claude/skills"),
        PathBuf::from(".kiro/skills"),
        PathBuf::from(".codex/skills"),
        home.join(".luma/skills"),
        home.join(".agents/skills"),
        home.join(".claude/skills"),
        home.join(".kiro/skills"),
        home.join(".codex/skills"),
        home.join(".config/luma/skills"),
    ]
}

/// Build catalog XML for system prompt.
pub fn build_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\nThe following skills provide specialized instructions for specific tasks.\n\
         When a task clearly matches a skill's description, use the Read tool with the \
         skill's `location` URI to load it. The loaded content will include the skill's \
         absolute directory path so you can reference companion files (scripts, templates, \
         etc.) directly.\n\n\
         <available_skills>\n",
    );
    for s in skills {
        out.push_str(&format!(
            "  <skill name=\"{}\">\n    <description>{}</description>\n    \
             <location>{}{}</location>\n  </skill>\n",
            s.name, s.description, SKILL_URI_PREFIX, s.name
        ));
    }
    out.push_str("</available_skills>\n");
    out
}

/// Validate skill name: `[A-Za-z0-9_.-]+`, not `.` or `..`.
pub fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Parse `artifact://skill/{name}` URI. Returns skill name or None.
pub fn parse_skill_read_path(path: &str) -> Option<String> {
    let name = path.strip_prefix(SKILL_URI_PREFIX)?;
    if name.is_empty() || name.contains('/') || !is_valid_skill_name(name) {
        return None;
    }
    Some(name.to_owned())
}

// --- internal ---

fn scan_recursive(
    dir: &Path,
    depth: usize,
    skills: &mut Vec<Skill>,
    seen: &mut std::collections::HashSet<String>,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let self_skill = dir.join(SKILL_FILE);
    if self_skill.is_file() {
        claim_skill(&self_skill, skills, seen);
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        scan_recursive(&entry.path(), depth + 1, skills, seen);
    }
}

fn claim_skill(
    skill_md: &Path,
    skills: &mut Vec<Skill>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Some(skill) = parse_skill_md(skill_md) else {
        crate::dbg_log!(
            "skill at {} skipped: unparseable frontmatter",
            skill_md.display()
        );
        return;
    };
    if !is_valid_skill_name(&skill.name) {
        crate::dbg_log!(
            "skill name {:?} at {} rejected",
            skill.name,
            skill_md.display()
        );
        return;
    }
    if !seen.insert(skill.name.clone()) {
        crate::dbg_log!("skill {:?} at {} shadowed", skill.name, skill_md.display());
        return;
    }
    skills.push(skill);
}

fn parse_skill_md(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    let frontmatter = extract_frontmatter(&content)?;
    let fields = parse_yaml_fields(frontmatter);

    let mut name = fields.get("name").cloned().unwrap_or_default();
    let description = fields.get("description").cloned().unwrap_or_default();

    // Fallback: derive name from containing directory (Codex convention).
    if name.is_empty()
        && let Some(parent) = path.parent().and_then(|p| p.file_name())
    {
        name = parent.to_string_lossy().into_owned();
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

fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let rest = trimmed.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    const FENCE: &str = "\n---";
    for (idx, _) in rest.match_indices(FENCE) {
        let after = &rest[idx + FENCE.len()..];
        let ok = after.is_empty()
            || after.starts_with('\n')
            || after.starts_with("\r\n")
            || after.starts_with(' ')
            || after.starts_with('\t');
        if ok {
            return Some(&rest[..idx]);
        }
    }
    None
}

fn parse_yaml_fields(frontmatter: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_end();
        if trimmed.is_empty() || raw.starts_with(' ') || raw.starts_with('\t') {
            i += 1;
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            i += 1;
            continue;
        };
        let key = trimmed[..colon].trim().to_owned();
        let value = trimmed[colon + 1..].trim();

        if value == ">" || value == "|" {
            let mut buf = String::new();
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if l.is_empty() {
                    buf.push('\n');
                    i += 1;
                    continue;
                }
                if !(l.starts_with(' ') || l.starts_with('\t')) {
                    break;
                }
                if !buf.is_empty() {
                    buf.push(if value == "|" { '\n' } else { ' ' });
                }
                buf.push_str(l.trim_start());
                i += 1;
            }
            out.insert(key, buf.trim().to_owned());
            continue;
        }

        let unquoted = strip_quotes(value);
        out.insert(key, unquoted.to_owned());
        i += 1;
    }
    out
}

fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("luma_skill_{prefix}_{pid}_{n}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parse_valid_skill() {
        let dir = unique_tmp("basic");
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
    fn parse_missing_description_rejected() {
        let dir = unique_tmp("missing_desc");
        let md = dir.join("SKILL.md");
        fs::write(&md, "---\nname: only-name\n---\nNo description.").unwrap();
        assert!(parse_skill_md(&md).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_missing_name_falls_back_to_folder() {
        let root = unique_tmp("fallback");
        let skill_dir = root.join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let md = skill_dir.join("SKILL.md");
        fs::write(&md, "---\ndescription: does things\n---\nbody").unwrap();
        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.name, "my-skill");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_quoted_values() {
        let dir = unique_tmp("quoted");
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: \"quoted-name\"\ndescription: 'single quoted'\n---\nx",
        )
        .unwrap();
        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.name, "quoted-name");
        assert_eq!(skill.description, "single quoted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_folded_block_scalar() {
        let dir = unique_tmp("folded");
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: folded\ndescription: >\n  first line\n  second line\n---\nbody",
        )
        .unwrap();
        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.description, "first line second line");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_literal_block_scalar() {
        let dir = unique_tmp("literal");
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: lit\ndescription: |\n  line A\n  line B\n---\nbody",
        )
        .unwrap();
        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.description, "line A\nline B");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_ignores_unknown_and_nested_fields() {
        let dir = unique_tmp("nested");
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: big\ndescription: handles things\nlicense: MIT\ncompatibility:\n  - claude-code\nmetadata:\n  version: 1.0\n---\nbody",
        )
        .unwrap();
        let skill = parse_skill_md(&md).unwrap();
        assert_eq!(skill.name, "big");
        assert_eq!(skill.description, "handles things");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_tolerates_bom() {
        let dir = unique_tmp("bom");
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "\u{feff}---\nname: bom-skill\ndescription: utf8 bom\n---\nbody",
        )
        .unwrap();
        assert_eq!(parse_skill_md(&md).unwrap().name, "bom-skill");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let dir = unique_tmp("nofm");
        let md = dir.join("SKILL.md");
        fs::write(&md, "# Just markdown, no frontmatter").unwrap();
        assert!(parse_skill_md(&md).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_recursive_nested_codex_layout() {
        let root = unique_tmp("recursive");
        let nested = root.join(".system").join("plan");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("SKILL.md"),
            "---\nname: plan\ndescription: planning\n---\nbody",
        )
        .unwrap();
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();
        scan_recursive(&root, 0, &mut skills, &mut seen);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "plan");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_recursive_stops_at_skill_root() {
        let root = unique_tmp("stop");
        fs::write(
            root.join("SKILL.md"),
            "---\nname: parent\ndescription: outer\n---\nbody",
        )
        .unwrap();
        let inner = root.join("scripts").join("nested");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            inner.join("SKILL.md"),
            "---\nname: child\ndescription: nope\n---\nbody",
        )
        .unwrap();
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();
        scan_recursive(&root, 0, &mut skills, &mut seen);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "parent");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_recursive_respects_depth_limit() {
        let root = unique_tmp("deep");
        let mut p = root.clone();
        for i in 0..(MAX_SCAN_DEPTH + 2) {
            p = p.join(format!("d{i}"));
        }
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("SKILL.md"),
            "---\nname: deep\ndescription: too deep\n---\nbody",
        )
        .unwrap();
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();
        scan_recursive(&root, 0, &mut skills, &mut seen);
        assert!(skills.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_precedence_first_wins() {
        let first = unique_tmp("first");
        let second = unique_tmp("second");
        for (root, desc) in [(&first, "first"), (&second, "second")] {
            let d = root.join("dup");
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("SKILL.md"),
                format!("---\nname: dup\ndescription: {desc}\n---\nbody"),
            )
            .unwrap();
        }
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();
        scan_recursive(&first, 0, &mut skills, &mut seen);
        scan_recursive(&second, 0, &mut skills, &mut seen);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "first");
        let _ = fs::remove_dir_all(&first);
        let _ = fs::remove_dir_all(&second);
    }

    #[test]
    fn discovery_roots_covers_all_cli_tools() {
        let roots = discovery_roots();
        let s: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
        assert!(s.iter().any(|p| p.ends_with(".agents/skills")));
        assert!(s.iter().any(|p| p.ends_with(".claude/skills")));
        assert!(s.iter().any(|p| p.ends_with(".kiro/skills")));
        assert!(s.iter().any(|p| p.ends_with(".codex/skills")));
    }

    #[test]
    fn catalog_format() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "A test".into(),
            path: PathBuf::from("/tmp/test/SKILL.md"),
        }];
        let catalog = build_catalog(&skills);
        assert!(catalog.contains("<location>artifact://skill/test</location>"));
        assert!(!catalog.contains("/tmp/test/SKILL.md"));
        // No <directory> or <assets> — those are gone.
        assert!(!catalog.contains("<directory>"));
        assert!(!catalog.contains("<assets>"));
    }

    #[test]
    fn empty_catalog() {
        assert!(build_catalog(&[]).is_empty());
    }

    #[test]
    fn valid_skill_names() {
        assert!(is_valid_skill_name("commit-work"));
        assert!(is_valid_skill_name("agent_browser"));
        assert!(is_valid_skill_name("v1.2"));
        assert!(is_valid_skill_name("PascalCase"));
    }

    #[test]
    fn invalid_skill_names() {
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("has space"));
        assert!(!is_valid_skill_name("has/slash"));
        assert!(!is_valid_skill_name(".."));
        assert!(!is_valid_skill_name("."));
        assert!(!is_valid_skill_name("unicode-é"));
    }

    #[test]
    fn parse_skill_read_canonical_uri() {
        assert_eq!(
            parse_skill_read_path("artifact://skill/commit-work"),
            Some("commit-work".into())
        );
    }

    #[test]
    fn parse_skill_read_rejects_bad() {
        assert!(parse_skill_read_path("artifact://skill/").is_none());
        assert!(parse_skill_read_path("artifact://skill/..").is_none());
        assert!(parse_skill_read_path("artifact://skill/a/b").is_none());
        assert!(parse_skill_read_path("/tmp/foo.rs").is_none());
        assert!(parse_skill_read_path("artifact://ev/ev_abc").is_none());
    }
}
