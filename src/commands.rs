use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// Command names `main.rs` routes itself (the built-in block, `main.rs:617-770`). Built-ins are
/// matched first and return before template expansion is ever reached, so a template file named
/// after one of these can never run. It is refused at load time and reported as a collision
/// instead: silently loading it would put an entry in `/commands` that does nothing when tapped,
/// and silently skipping it would leave the owner with a file they believe is installed.
///
/// `builtin_commands_are_all_reserved` in `main.rs` reads the routing block back out of the source
/// and fails if a built-in is added without being listed here.
pub const RESERVED: &[&str] = &[
    "audit",
    "commands",
    "context",
    "delete",
    "forget",
    "jobs",
    "memory",
    "model",
    "models",
    "new",
    "provider",
    "providers",
    "reminders",
    "resume",
    "rtk",
    "sessions",
    "status",
    "workspace",
];

pub struct CommandTemplate {
    pub name: String,
    pub description: Option<String>,
    body: String,
}

/// A template file that was found but not loaded, because a built-in command of the same name
/// would always win the match.
pub struct ShadowedCommand {
    pub name: String,
    pub path: PathBuf,
}

/// Every usable template, plus every file that collides with a built-in. The collisions travel
/// with the list rather than being dropped so that `/commands` and the startup log can both name
/// them.
pub struct CommandSet {
    pub templates: Vec<CommandTemplate>,
    pub shadowed: Vec<ShadowedCommand>,
}

pub fn list(workspace: &Path) -> Result<CommandSet> {
    list_with_global(&global_dir()?, workspace)
}

fn list_with_global(global: &Path, workspace: &Path) -> Result<CommandSet> {
    let mut commands = BTreeMap::new();
    let mut shadowed = BTreeMap::new();
    load_dir(global, &mut commands, &mut shadowed)?;
    load_dir(
        &workspace.join(".kumo/commands"),
        &mut commands,
        &mut shadowed,
    )?;
    Ok(CommandSet {
        templates: commands.into_values().collect(),
        shadowed: shadowed.into_values().collect(),
    })
}

/// True when `name` is a built-in command, and so unusable as a template name.
pub fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

pub fn expand(input: &str, workspace: &Path) -> Result<Option<String>> {
    expand_with_global(input, &global_dir()?, workspace)
}

fn expand_with_global(input: &str, global: &Path, workspace: &Path) -> Result<Option<String>> {
    let Some(invocation) = invocation(input) else {
        return Ok(None);
    };
    let command = list_with_global(global, workspace)?
        .templates
        .into_iter()
        .find(|command| command.name == invocation.name);
    Ok(command.map(|command| {
        if command.body.contains("{{args}}") {
            command.body.replace("{{args}}", invocation.args)
        } else if invocation.args.is_empty() {
            command.body
        } else {
            format!("{}\n\nArguments: {}", command.body, invocation.args)
        }
    }))
}

fn global_dir() -> Result<std::path::PathBuf> {
    let config = crate::config::path()?;
    Ok(config
        .parent()
        .context("config path has no parent directory")?
        .join("commands"))
}

fn load_dir(
    path: &Path,
    commands: &mut BTreeMap<String, CommandTemplate>,
    shadowed: &mut BTreeMap<String, ShadowedCommand>,
) -> Result<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not list {}", path.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !valid_name(name) {
            continue;
        }
        // Refused, not loaded: `/status` would reach the built-in and never this file, so the
        // honest outcome is a named collision rather than a menu entry that silently does nothing.
        if is_reserved(name) {
            shadowed.insert(
                name.to_owned(),
                ShadowedCommand {
                    name: name.to_owned(),
                    path: path.clone(),
                },
            );
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("could not read command template {}", path.display()))?;
        let (description, body) = split_frontmatter(&content);
        if body.trim().is_empty() {
            continue;
        }
        commands.insert(
            name.to_owned(),
            CommandTemplate {
                name: name.to_owned(),
                description,
                body: body.trim().to_owned(),
            },
        );
    }
    Ok(())
}

fn split_frontmatter(content: &str) -> (Option<String>, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (None, content);
    };
    let Some((frontmatter, body)) = rest.split_once("\n---\n") else {
        return (None, content);
    };
    let description = frontmatter.lines().find_map(|line| {
        line.strip_prefix("description:")
            .map(|value| value.trim().trim_matches(['\'', '"']).to_owned())
            .filter(|value| !value.is_empty())
    });
    (description, body)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

struct Invocation<'a> {
    name: &'a str,
    args: &'a str,
}

fn invocation(input: &str) -> Option<Invocation<'_>> {
    let (head, args) = input.trim().split_once(' ').unwrap_or((input.trim(), ""));
    let name = head.strip_prefix('/')?.split('@').next()?;
    valid_name(name).then_some(Invocation {
        name,
        args: args.trim(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn local_template_expands_arguments_and_frontmatter() {
        let root = std::env::temp_dir().join(format!("kumo-commands-{}", Uuid::new_v4()));
        let directory = root.join(".kumo/commands");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("review.md"),
            "---\ndescription: Review a target\n---\nReview {{args}} carefully.",
        )
        .unwrap();

        let global = root.join("global-commands");
        let expanded = expand_with_global("/review src/main.rs", &global, &root)
            .unwrap()
            .unwrap();
        assert_eq!(expanded, "Review src/main.rs carefully.");
        let listed = list_with_global(&global, &root).unwrap();
        let review = listed
            .templates
            .iter()
            .find(|item| item.name == "review")
            .unwrap();
        assert_eq!(review.description.as_deref(), Some("Review a target"));
        assert!(listed.shadowed.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_like_command_names() {
        assert!(invocation("/../../secret").is_none());
        assert!(invocation("normal text").is_none());
    }

    /// A template named after a built-in can never be reached — `/status` is answered by the
    /// built-in block long before template expansion runs. It must therefore come back as a named
    /// collision, not as a loaded command that quietly never runs.
    #[test]
    fn a_template_named_after_a_builtin_is_reported_not_loaded() {
        let root = std::env::temp_dir().join(format!("kumo-commands-{}", Uuid::new_v4()));
        let directory = root.join(".kumo/commands");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("status.md"), "Summarise the host status.").unwrap();
        fs::write(directory.join("standup.md"), "Write a standup note.").unwrap();

        let global = root.join("global-commands");
        let listed = list_with_global(&global, &root).unwrap();

        assert!(
            !listed.templates.iter().any(|item| item.name == "status"),
            "a shadowed template must not be offered as if it worked"
        );
        assert!(listed.templates.iter().any(|item| item.name == "standup"));
        let shadowed = listed
            .shadowed
            .iter()
            .find(|item| item.name == "status")
            .expect("the collision has to be reported");
        assert!(shadowed.path.ends_with("status.md"), "{:?}", shadowed.path);
        // Expansion agrees with the listing: the file is not silently preferred either.
        assert!(
            expand_with_global("/status", &global, &root)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The collision is about the *name*, not the directory: a global template is refused for the
    /// same reason a workspace one is.
    #[test]
    fn a_global_template_named_after_a_builtin_is_reported_too() {
        let root = std::env::temp_dir().join(format!("kumo-commands-{}", Uuid::new_v4()));
        let global = root.join("global-commands");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join("jobs.md"), "List my jobs.").unwrap();

        let listed = list_with_global(&global, &root).unwrap();

        assert!(listed.templates.is_empty());
        assert_eq!(listed.shadowed.len(), 1);
        assert_eq!(listed.shadowed[0].name, "jobs");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reserved_names_are_sorted_and_unique() {
        let mut sorted = RESERVED.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted, RESERVED,
            "keep RESERVED sorted and free of duplicates"
        );
        assert!(is_reserved("status"));
        assert!(!is_reserved("standup"));
    }
}
