//! skills.rs — markdown skill files with progressive disclosure (port of
//! skills.py).
//!
//! Convention: every `*.md` in `<workdir>/[agent] skills_dir` is a skill.
//! Frontmatter carries `name` + `description` — the SHORT advertisement is
//! all the model ever sees in its system prompt. The body loads only when
//! the model calls `use_skill` (recorded as a `skill_used` journal event).
//! Startup lint warns on oversized descriptions/bodies and missing evals
//! (operator-facing, stderr — never model context).

use std::path::PathBuf;

/// One parsed skill file.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub description_chars: usize,
    pub body_lines: usize,
    pub uses: usize,
}

/// The frontmatter we consume (name + description); everything else is
/// ignored, like the POC parser.
#[derive(Debug)]
struct Frontmatter {
    name: Option<String>,
    description: String,
}

/// Parse `---`-delimited frontmatter: returns (frontmatter, body).
/// Without frontmatter the whole text is the body.
fn parse_frontmatter(text: &str) -> (Frontmatter, String) {
    if !text.starts_with("---") {
        return (
            Frontmatter {
                name: None,
                description: String::new(),
            },
            text.to_string(),
        );
    }
    let rest = &text[3..]; // past the opening ---
    let end = rest.find("\n---");
    match end {
        None => (
            Frontmatter {
                name: None,
                description: String::new(),
            },
            text.to_string(),
        ),
        Some(i) => {
            let fm_block = rest[0..i].to_string();
            let body = rest[i + 4..].trim().to_string();
            let mut name: Option<String> = None;
            let mut description = String::new();
            for line in fm_block.split('\n') {
                let line = line.trim();
                let ci = line.find(':');
                if let Some(ci) = ci {
                    let key = line[0..ci].trim();
                    let val = line[ci + 1..].trim().trim_matches('"').trim_matches('\'');
                    match key {
                        "name" => name = Some(val.to_string()),
                        "description" => description = val.to_string(),
                        _ => {}
                    }
                }
            }
            (Frontmatter { name, description }, body)
        }
    }
}

/// Load + parse all skill `.md` files (name + description + body).
pub fn load_skills(dir: &PathBuf) -> Vec<Skill> {
    if !dir.exists() {
        return vec![];
    }
    let mut out: Vec<Skill> = vec![];
    match dir.read_dir() {
        Err(_) => return vec![],
        Ok(rd) => {
            for entry in rd.flatten() {
                let file_name = entry.file_name();
                let fname = file_name.to_str().unwrap_or_default().to_string();
                if !fname.ends_with(".md") {
                    continue;
                }
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                let (fm, body) = parse_frontmatter(text.as_str());
                let name = fm.name.unwrap_or(fname[0..fname.len() - 3].to_string());
                let body_lines = body.split('\n').count();
                out.push(Skill {
                    name,
                    description: fm.description.clone(),
                    body,
                    description_chars: fm.description.len(),
                    body_lines,
                    uses: 0,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The lint result: warnings (operator-facing) + hard errors.
#[derive(Debug)]
pub struct LintResult {
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// Startup lint — oversized desc/body and missing evals warn, empty
/// description is a hard error (unadvertisable skill).
pub fn lint_skills(
    skills: &[Skill],
    evals_dir: &PathBuf,
    max_desc_chars: usize,
    max_body_lines: usize,
) -> LintResult {
    let mut eval_names: Vec<String> = vec![];
    if evals_dir.exists() {
        for entry in evals_dir
            .read_dir()
            .unwrap_or_else(|_| panic!("read evals dir"))
            .flatten()
        {
            let file_name = entry.file_name();
            let fname = file_name.to_str().unwrap_or_default().to_string();
            if fname.ends_with(".md") {
                eval_names.push(fname[0..fname.len() - 3].to_string());
            }
        }
    }
    let mut res = LintResult {
        warnings: Vec::new(),
        errors: Vec::new(),
    };
    for s in skills {
        let n: String = s.name.clone();
        if s.description.is_empty() {
            res.errors.push(format!(
                "skill '{n}': empty description (frontmatter required)"
            ));
        }
        if s.description_chars > max_desc_chars {
            res.warnings.push(format!(
                "skill '{n}': description {}ch > {max_desc_chars} (only the description enters context — keep it short)",
                s.description_chars
            ));
        }
        if s.body_lines > max_body_lines {
            res.warnings.push(format!(
                "skill '{n}': body {} lines > {max_body_lines} (loaded on demand, but keep lean)",
                s.body_lines
            ));
        }
        let has_eval = eval_names.iter().any(|e| e.as_str() == n.as_str());
        if !has_eval {
            res.warnings.push(format!(
                "skill '{n}': no matching eval (expected evals/{n}.md) — untested skills are unproven"
            ));
        }
    }
    res
}

/// Index of a skill by name, or None.
pub fn find_skill(skills: &[Skill], name: &str) -> Option<usize> {
    for i in 0..skills.len() {
        if skills[i].name.as_str() == name {
            return Some(i);
        }
    }
    None
}

/// The ONLY thing about skills that enters the system prompt: name + desc.
pub fn catalog_block(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("\nAvailable skills (call use_skill '<name>' to load the full one):\n");
    for s in skills {
        let n: String = s.name.clone();
        let d: String = s.description.clone();
        out.push_str(format!("  - {n}: {d}\n").to_string().as_str());
    }
    out
}

/// The short advertisement for one skill.
pub fn advertise(s: &Skill) -> String {
    let n: String = s.name.clone();
    let d: String = s.description.clone();
    format!("- {n}: {d}").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_skills(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir()
            .join(format!("curam-h6-{tag}"))
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&base.join("skills")).unwrap();
        std::fs::create_dir_all(&base.join("evals")).unwrap();
        (base.join("skills"), base.join("evals"), base)
    }

    #[test]
    fn loads_frontmatter_and_discloses_progressively() {
        let (sk, ev, base) = tmp_skills("load");
        std::fs::write(
            &sk.join("git-workflow.md"),
            "---\nname: git-workflow\ndescription: Always commit + tag after a task.\n---\nFull git workflow instructions...\n".as_bytes(),
        )
        .unwrap();
        std::fs::write(
            &ev.join("git-workflow.md"),
            "eval: covered by eval_tasks/git_workflow\n".as_bytes(),
        )
        .unwrap();

        let skills = load_skills(&sk);
        let mut names: Vec<&str> = vec![];
        for skk in &skills {
            names.push(skk.name.as_str());
        }
        assert_eq!(skills.len(), 1, "expected exactly 1 skill, got: {names:?}");
        let catalog = catalog_block(&skills);
        assert!(
            catalog.contains("Always commit + tag"),
            "description advertised"
        );
        assert!(
            !catalog.contains("Full git workflow"),
            "body stays out of the prompt"
        );

        let res = lint_skills(&skills, &ev, 240, 200);
        assert_eq!(res.errors.len(), 0);
        assert_eq!(
            res.warnings
                .iter()
                .filter(|w| w.contains("no matching eval"))
                .count(),
            0
        );
        let i = find_skill(&skills, "git-workflow").unwrap();
        assert_eq!(skills[i].body_lines, 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn lint_flags_oversized_and_unevaluated() {
        let (sk, ev, base) = tmp_skills("lint");
        let mut desc = String::new();
        for _ in 0..300 {
            desc.push_str("x");
        }
        let mut body = String::new();
        for _ in 0..250 {
            body.push_str("line\n");
        }
        let raw = format!(
            "---\nname: big\ndescription: {}\n---\n{}",
            desc.clone(),
            body
        );
        std::fs::write(&sk.join("big.md"), raw.as_bytes()).unwrap();

        let skills = load_skills(&sk);
        assert_eq!(skills.len(), 1);
        let res = lint_skills(&skills, &ev, 240, 200);
        assert!(
            res.warnings.len() >= 2,
            "desc + body both over the line: {res:?}"
        );

        // empty description is a hard error
        std::fs::write(
            &sk.join("silent.md"),
            "---\nname: silent\n---\nBody only.\n".as_bytes(),
        )
        .unwrap();
        let skills2 = load_skills(&sk);
        let res2 = lint_skills(&skills2, &ev, 240, 200);
        assert!(res2.errors.iter().any(|e| e.contains("empty description")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn frontmatter_defaults_to_filename() {
        let (sk, _, base) = tmp_skills("default");
        std::fs::write(
            &sk.join("bare.md"),
            "No frontmatter at all — just prose.\n".as_bytes(),
        )
        .unwrap();
        let skills = load_skills(&sk);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "bare");
        assert!(skills[0].body.contains("frontmatter"));
        std::fs::remove_dir_all(&base).ok();
    }
}
