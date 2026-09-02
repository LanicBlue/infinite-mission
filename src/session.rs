use anyhow::{bail, Context, Result};
use std::path::Path;

/// Write a session token file for an agent.
pub fn write_token(sessions_dir: &Path, agent_id: &str, token: &str) -> Result<()> {
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("failed to create {}", sessions_dir.display()))?;
    let path = sessions_dir.join(agent_id);
    std::fs::write(&path, token)
        .with_context(|| format!("failed to write session file: {}", path.display()))?;
    Ok(())
}

/// Read a session token file. Returns None if file does not exist.
pub fn read_token(sessions_dir: &Path, agent_id: &str) -> Result<Option<String>> {
    let path = sessions_dir.join(agent_id);
    if !path.exists() {
        return Ok(None);
    }
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read session file: {}", path.display()))?;
    Ok(Some(token))
}

/// Delete a session token file.
pub fn delete_token(sessions_dir: &Path, agent_id: &str) -> Result<()> {
    let path = sessions_dir.join(agent_id);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Delete all session token files.
pub fn delete_all(sessions_dir: &Path) -> Result<()> {
    if sessions_dir.exists() {
        for entry in std::fs::read_dir(sessions_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Validate that the local token matches the expected token (from DB).
/// Returns Ok(()) if they match or if no local session file exists (backward compat).
/// Errors with "Session replaced" if the local token differs from expected.
pub fn validate(sessions_dir: &Path, agent_id: &str, expected_token: &str) -> Result<()> {
    let current = read_token(sessions_dir, agent_id)?;
    match current {
        None => Ok(()), // No session file = agent joined before this feature, skip
        Some(token) if token == expected_token => Ok(()),
        Some(_) => bail!(
            "Session replaced. Another terminal joined as {agent_id}. \
             Re-join with a different ID (e.g. im join {agent_id}-2 --role <your-role>)."
        ),
    }
}
