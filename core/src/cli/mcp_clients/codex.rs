//! Codex MCP client adapter.
//!
//! Codex stores MCP servers in `~/.codex/config.toml` under
//! `mcp_servers.<name>`. This adapter validates the full TOML document before
//! mutation, preserves unrelated settings, and uses the shared atomic writer.

use super::{
    backup_path_for, write_config_text_atomic, ClientId, InstallReceipt, McpClient, McpServerEntry,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{value, Array, DocumentMut, Item, Table};

pub struct Codex;

impl McpClient for Codex {
    fn id(&self) -> ClientId {
        ClientId::Codex
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".codex").join("config.toml")
    }

    fn is_installed(&self, home: &Path) -> Result<bool> {
        let path = self.config_path(home);
        if !path.exists() {
            return Ok(false);
        }
        let root = read_config(&path)?;
        Ok(root
            .get("mcp_servers")
            .and_then(Item::as_table_like)
            .and_then(|servers| servers.get("localmem"))
            .is_some())
    }

    fn install(&self, home: &Path, entry: &McpServerEntry) -> Result<InstallReceipt> {
        let config_path = self.config_path(home);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config directory at {}", parent.display()))?;
        }

        let backup_path = backup_path_for(&config_path);
        let mut root = match read_config(&config_path) {
            Ok(root) => root,
            Err(error) => {
                if config_path.exists() {
                    back_up(&config_path, &backup_path)?;
                }
                return Err(error);
            }
        };
        let mut rendered = render_entry(entry);
        let servers = root
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            })
            .as_table_like_mut()
            .context("`mcp_servers` must be a TOML table")?;

        if servers
            .get(&entry.name)
            .is_some_and(|server| matches_entry(server, entry))
        {
            return Ok(InstallReceipt {
                config_path,
                backup_path,
            });
        }

        if config_path.exists() {
            back_up(&config_path, &backup_path)?;
        }
        if let (Some(previous), Some(replacement)) = (
            servers.get(&entry.name).and_then(Item::as_table),
            rendered.as_table_mut(),
        ) {
            // Preserve the block's comments and position among unrelated tables.
            *replacement.decor_mut() = previous.decor().clone();
            if let Some(position) = previous.position() {
                replacement.set_position(position);
            }
        }
        servers.insert(&entry.name, rendered);
        let serialized = root.to_string();
        write_config_text_atomic(&config_path, &serialized, "config.toml")?;

        Ok(InstallReceipt {
            config_path,
            backup_path,
        })
    }

    fn uninstall(&self, home: &Path) -> Result<bool> {
        let config_path = self.config_path(home);
        if !config_path.exists() {
            return Ok(false);
        }
        let mut root = read_config(&config_path)?;
        let removed = root
            .get_mut("mcp_servers")
            .and_then(Item::as_table_like_mut)
            .map(|servers| servers.remove("localmem").is_some())
            .unwrap_or(false);
        if removed {
            let backup_path = backup_path_for(&config_path);
            back_up(&config_path, &backup_path)?;
            let serialized = root.to_string();
            write_config_text_atomic(&config_path, &serialized, "config.toml")?;
        }
        Ok(removed)
    }
}

fn back_up(config_path: &Path, backup_path: &Path) -> Result<()> {
    fs::copy(config_path, backup_path).with_context(|| {
        format!(
            "back up {} to {} before mutating",
            config_path.display(),
            backup_path.display()
        )
    })?;
    Ok(())
}

fn read_config(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read existing config at {}", path.display()))?;
    raw.parse().with_context(|| {
        format!(
            "parse existing config at {} as TOML (refusing to clobber a malformed file)",
            path.display()
        )
    })
}

fn render_entry(entry: &McpServerEntry) -> Item {
    let mut server = Table::new();
    server.insert("command", value(&entry.command));
    if !entry.args.is_empty() {
        let mut args = Array::new();
        for arg in &entry.args {
            args.push(arg.as_str());
        }
        server.insert("args", value(args));
    }
    if !entry.env.is_empty() {
        let mut env = Table::new();
        for (key, val) in &entry.env {
            env.insert(key, value(val));
        }
        server.insert("env", Item::Table(env));
    }
    Item::Table(server)
}

// Compare values rather than TOML decoration so an equivalent user-formatted
// entry remains a no-op and does not overwrite the original backup.
fn matches_entry(server: &Item, entry: &McpServerEntry) -> bool {
    let Some(server) = server.as_table_like() else {
        return false;
    };
    let expected_len = 1 + usize::from(!entry.args.is_empty()) + usize::from(!entry.env.is_empty());
    if server.len() != expected_len
        || server.get("command").and_then(Item::as_str) != Some(entry.command.as_str())
    {
        return false;
    }
    if !entry.args.is_empty() {
        let Some(args) = server.get("args").and_then(Item::as_array) else {
            return false;
        };
        if args.len() != entry.args.len()
            || !args
                .iter()
                .zip(&entry.args)
                .all(|(a, b)| a.as_str() == Some(b.as_str()))
        {
            return false;
        }
    }
    if !entry.env.is_empty() {
        let Some(env) = server.get("env").and_then(Item::as_table_like) else {
            return false;
        };
        if env.len() != entry.env.len()
            || !entry
                .env
                .iter()
                .all(|(key, val)| env.get(key).and_then(Item::as_str) == Some(val.as_str()))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn entry() -> McpServerEntry {
        McpServerEntry {
            name: "localmem".into(),
            command: "/usr/local/bin/bun".into(),
            args: vec!["/opt/localmem/mcp-server/src/index.ts".into()],
            env: BTreeMap::from([("LOCALMEM_CORE_URL".into(), "http://127.0.0.1:7788".into())]),
        }
    }

    #[test]
    fn install_and_uninstall_preserve_comments_and_key_order() {
        for original in [
            "# My Codex settings\nmodel = 'gpt-5' # preferred model\n\n# Keep approval policy\napproval_policy = \"on-request\"\n\n[mcp_servers.other]\n# Other server settings\ncommand = 'other-server'\nargs = [ 'one', 'two' ] # retain spacing\n",
            "# My Codex settings\nmodel = 'gpt-5'\napproval_policy = 'on-request'\n",
            "# Empty config with a comment\n\n",
        ] {
            let home = tempdir().unwrap();
            let config = Codex.config_path(home.path());
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(&config, original).unwrap();

            Codex.install(home.path(), &entry()).unwrap();
            let installed = fs::read_to_string(&config).unwrap();
            // Even a comment-only document retains all of its original bytes.
            assert!(installed.contains(original), "{installed}");
            assert!(Codex.is_installed(home.path()).unwrap());
            let backup = fs::read(backup_path_for(&config)).unwrap();
            Codex.install(home.path(), &entry()).unwrap();
            assert_eq!(fs::read_to_string(&config).unwrap(), installed);
            assert_eq!(fs::read(backup_path_for(&config)).unwrap(), backup);

            assert!(Codex.uninstall(home.path()).unwrap());
            assert_eq!(fs::read_to_string(&config).unwrap(), original);
        }
    }

    #[test]
    fn equivalent_user_formatted_entry_is_a_no_op() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "# Keep this formatting\n[mcp_servers.localmem]\nargs = [ '/opt/localmem/mcp-server/src/index.ts' ] # entry point\ncommand = '/usr/local/bin/bun'\nenv = { LOCALMEM_CORE_URL = 'http://127.0.0.1:7788' }\n";
        fs::write(&config, original).unwrap();
        Codex.install(home.path(), &entry()).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert!(!backup_path_for(&config).exists());
    }

    #[test]
    fn inline_servers_remain_valid_through_install_and_uninstall() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "mcp_servers = { other = { command = 'other' } }\n").unwrap();
        Codex.install(home.path(), &entry()).unwrap();
        assert!(Codex.is_installed(home.path()).unwrap());
        Codex.install(home.path(), &entry()).unwrap();
        assert!(Codex.uninstall(home.path()).unwrap());
        let parsed = read_config(&config).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
    }

    #[test]
    fn installs_codex_entry_and_preserves_unrelated_settings() {
        let home = tempdir().unwrap();
        let config = home.path().join(".codex").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "model = \"gpt-5\"\n").unwrap();

        Codex.install(home.path(), &entry()).unwrap();

        let parsed = read_config(&config).unwrap();
        assert_eq!(parsed["model"].as_str(), Some("gpt-5"));
        let localmem = parsed["mcp_servers"]["localmem"].as_table().unwrap();
        assert_eq!(localmem["command"].as_str(), Some("/usr/local/bin/bun"));
        assert_eq!(
            localmem["env"]["LOCALMEM_CORE_URL"].as_str(),
            Some("http://127.0.0.1:7788")
        );
        assert!(Codex.is_installed(home.path()).unwrap());
    }

    #[test]
    fn repeated_install_is_a_no_op() {
        let home = tempdir().unwrap();
        Codex.install(home.path(), &entry()).unwrap();
        let config = Codex.config_path(home.path());
        let first = fs::read(&config).unwrap();
        let backup = backup_path_for(&config);
        assert!(!backup.exists());

        Codex.install(home.path(), &entry()).unwrap();

        assert_eq!(fs::read(config).unwrap(), first);
        assert!(!backup.exists());
    }

    #[test]
    fn backs_up_existing_config_before_mutating() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = b"model = \"gpt-5\"\n";
        fs::write(&config, original).unwrap();

        let receipt = Codex.install(home.path(), &entry()).unwrap();

        assert_eq!(fs::read(receipt.backup_path).unwrap(), original);
    }

    #[test]
    fn malformed_config_is_backed_up_but_never_clobbered() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let malformed = b"[mcp_servers.localmem\ncommand = \"bun\"";
        fs::write(&config, malformed).unwrap();

        let error = Codex.install(home.path(), &entry()).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to clobber"));
        assert_eq!(fs::read(&config).unwrap(), malformed);
        assert_eq!(fs::read(backup_path_for(&config)).unwrap(), malformed);
    }

    #[test]
    fn uninstall_removes_only_localmem() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "[mcp_servers.other]\ncommand = \"other-server\"\n\n[mcp_servers.localmem]\ncommand = \"bun\"\n",
        )
        .unwrap();

        assert!(Codex.uninstall(home.path()).unwrap());
        assert!(!Codex.is_installed(home.path()).unwrap());
        let parsed = read_config(&config).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("other-server")
        );
        let backup = fs::read_to_string(backup_path_for(&config)).unwrap();
        assert!(backup.contains("[mcp_servers.localmem]"));
    }

    #[test]
    fn updating_entry_preserves_surrounding_tables() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let before = concat!(
            "# Settings\nmodel = 'gpt-5'\n\n",
            "[mcp_servers.other]\ncommand = 'other-server'\n\n",
            "[profiles.work]\nmodel = 'work-model'\n",
        );
        let block = "\n# LocalMem settings\n[mcp_servers.localmem]\ncommand = 'old-command'\n";
        let after = "\n# Another profile\n[profiles.personal]\nmodel = 'personal-model'\n";
        let original = format!("{before}{block}{after}");
        fs::write(&config, &original).unwrap();

        Codex.install(home.path(), &entry()).unwrap();

        let installed = fs::read_to_string(&config).unwrap();
        assert!(installed.starts_with(before), "{installed}");
        assert!(installed.ends_with(after), "{installed}");
        assert_eq!(
            read_config(&config).unwrap()["mcp_servers"]["localmem"]["command"].as_str(),
            Some(entry().command.as_str())
        );
        assert_eq!(
            fs::read_to_string(backup_path_for(&config)).unwrap(),
            original
        );
        assert!(installed.contains("# LocalMem settings\n[mcp_servers.localmem]"));
        assert!(Codex.uninstall(home.path()).unwrap());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            format!("{before}{after}")
        );
    }
}
