use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};

pub struct CommandTemplate {
    pub name: String,
    pub description: Option<String>,
    body: String,
}

pub fn list(workspace: &Path) -> Result<Vec<CommandTemplate>> {
    list_with_global(&global_dir()?, workspace)
}

fn list_with_global(global: &Path, workspace: &Path) -> Result<Vec<CommandTemplate>> {
    let mut commands = BTreeMap::new();
    load_dir(global, &mut commands)?;
    load_dir(&workspace.join(".kumo/commands"), &mut commands)?;
    Ok(commands.into_values().collect())
}

pub fn expand(input: &str, workspace: &Path) -> Result<Option<String>> {
    expand_with_global(input, &global_dir()?, workspace)
}

fn expand_with_global(input: &str, global: &Path, workspace: &Path) -> Result<Option<String>> {
    let Some(invocation) = invocation(input) else {
        return Ok(None);
    };
    let command = list_with_global(global, workspace)?
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

fn load_dir(path: &Path, commands: &mut BTreeMap<String, CommandTemplate>) -> Result<()> {
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
        let review = listed.iter().find(|item| item.name == "review").unwrap();
        assert_eq!(review.description.as_deref(), Some("Review a target"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_like_command_names() {
        assert!(invocation("/../../secret").is_none());
        assert!(invocation("normal text").is_none());
    }
}
