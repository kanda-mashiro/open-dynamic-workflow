//! Agent-type registry: `agentType: "<name>"` loads `<agents_dir>/<name>.md`
//! and prepends its markdown body to the agent's prompt as system framing
//! (codex exec has no separate system-prompt flag). YAML frontmatter is
//! discarded by hand — no yaml dependency for a block we only need to skip.

use std::path::{Path, PathBuf};

/// Registry location: `$CODEX_FLOW_AGENTS_DIR` > `~/.codex-flow/agents`.
pub fn agents_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CODEX_FLOW_AGENTS_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex-flow").join("agents")
}

/// Strip a leading `---\n…\n---\n` frontmatter block. Anything else — no block,
/// or an unterminated one — is returned whole (content must never be lost).
pub fn strip_frontmatter(content: &str) -> &str {
    let rest = if let Some(r) = content.strip_prefix("---\n") {
        r
    } else if let Some(r) = content.strip_prefix("---\r\n") {
        r
    } else {
        return content;
    };
    if let Some(pos) = rest.find("\n---\n") {
        return &rest[pos + 5..];
    }
    if let Some(pos) = rest.find("\n---\r\n") {
        return &rest[pos + 6..];
    }
    // A closing fence at EOF (no trailing newline) closes an empty-body block —
    // returning `content` here would leak YAML into the prompt (codex M6 #4).
    if rest.ends_with("\n---") || rest.ends_with("\n---\r") {
        return "";
    }
    content
}

/// System prefix for an agent type: the body of `<dir>/<type>.md`. A missing
/// file is an ERROR — a typo'd type must not silently run with no framing.
pub fn agent_system_prefix(agent_type: &str, dir: &Path) -> Result<String, String> {
    if agent_type.is_empty()
        || agent_type.contains('/')
        || agent_type.contains('\\')
        || agent_type.contains("..")
    {
        return Err(format!("agentType {agent_type:?}: invalid type name"));
    }
    let path = dir.join(format!("{agent_type}.md"));
    match std::fs::read_to_string(&path) {
        Ok(c) => Ok(strip_frontmatter(&c).trim().to_string()),
        Err(e) => Err(format!(
            "agentType {agent_type:?}: cannot read {}: {e}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_stripped_body_kept() {
        let md = "---\nname: reviewer\ndescription: x\n---\nYou are strict.\nFind bugs.";
        assert_eq!(strip_frontmatter(md), "You are strict.\nFind bugs.");
    }

    #[test]
    fn no_frontmatter_returned_whole() {
        assert_eq!(strip_frontmatter("Just a body"), "Just a body");
        // A later --- is body content, not frontmatter.
        let md = "Body first\n---\nstill body";
        assert_eq!(strip_frontmatter(md), md);
    }

    #[test]
    fn unterminated_frontmatter_not_swallowed() {
        let md = "---\nname: x\nno closing fence";
        assert_eq!(strip_frontmatter(md), md);
    }

    #[test]
    fn closing_fence_at_eof_yields_empty_body() {
        assert_eq!(strip_frontmatter("---\nname: x\n---"), "");
        assert_eq!(strip_frontmatter("---\r\nname: x\r\n---\r"), "");
    }

    #[test]
    fn prefix_hit_missing_and_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "---\nname: reviewer\n---\nYou are a strict reviewer.\n",
        )
        .unwrap();
        assert_eq!(
            agent_system_prefix("reviewer", dir.path()).unwrap(),
            "You are a strict reviewer."
        );
        assert!(agent_system_prefix("nope", dir.path()).is_err(), "typo errors");
        assert!(
            agent_system_prefix("../evil", dir.path()).is_err(),
            "traversal refused"
        );
    }
}
