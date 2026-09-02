use crate::init;
use anyhow::Result;
use std::path::Path;

pub fn list_roles(workspace: &Path) -> Vec<String> {
    let dir = workspace.join(".im").join("roles");
    let mut roles: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().map(|e| e == "md").unwrap_or(false))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(String::from)
        })
        .collect();
    roles.sort();
    roles
}

pub fn load_role(workspace: &Path, role: &str) -> Result<String> {
    let path = workspace.join(".im").join("roles").join(format!("{role}.md"));
    let prompt = std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("no role file for '{role}'; known roles: {}", list_roles(workspace).join(", "))
    })?;
    Ok(prompt)
}

pub fn builtin_roles() -> &'static [&'static str] {
    init::BUILTIN_ROLES
}
