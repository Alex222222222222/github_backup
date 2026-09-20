use std::{path::Path, sync::Arc};

use anyhow::Context;
use russh::{
    Disconnect, client, keys,
    keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use russh_sftp::client::SftpSession;

use crate::config::SshConfig;

pub async fn list_remote_directories(config: &SshConfig) -> anyhow::Result<Vec<String>> {
    let host = config.host.clone();
    let username = config.username.clone();
    let private_key_path = config.private_key_path.clone();
    let root_dir = config.root_dir.clone();

    let mut session = client::connect(
        Arc::new(client::Config::default()),
        (host.as_str(), 22),
        KnownHostsHandler { host: host.clone() },
    )
    .await
    .with_context(|| format!("failed to connect to SSH host {host}"))?;

    let private_key = keys::load_secret_key(&private_key_path, None).with_context(|| {
        format!(
            "failed to load SSH private key {}",
            private_key_path.display()
        )
    })?;
    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    let authentication = session
        .authenticate_publickey(
            username.clone(),
            PrivateKeyWithHashAlg::new(Arc::new(private_key), rsa_hash),
        )
        .await
        .with_context(|| format!("failed to authenticate SSH user {username}"))?;
    if !authentication.success() {
        anyhow::bail!("SSH public-key authentication failed for user {username}");
    }

    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream()).await?;
    let mut entries = sftp
        .read_dir(root_dir.clone())
        .await
        .with_context(|| format!("failed to list remote SSH directory {root_dir}"))?;

    let mut directories = Vec::new();
    for entry in &mut entries {
        if entry.file_type().is_dir() {
            directories.push(entry.file_name());
        }
    }
    directories.sort();

    sftp.close().await?;
    session
        .disconnect(Disconnect::ByApplication, "backup complete", "en")
        .await?;

    Ok(directories)
}

pub fn remote_repo_path(root_dir: &str, repo_name: &str) -> String {
    let root_dir = root_dir.trim_end_matches('/');
    if root_dir.is_empty() {
        format!("/{repo_name}")
    } else {
        format!("{root_dir}/{repo_name}")
    }
}

pub fn git_remote_url(config: &SshConfig, repo_name: &str) -> String {
    format!(
        "{}@{}:{}",
        config.username,
        config.host,
        remote_repo_path(&config.root_dir, repo_name)
    )
}

pub fn configure_git_ssh(command: &mut tokio::process::Command, private_key_path: &Path) {
    command
        .env(
            "GIT_SSH_COMMAND",
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes",
                shell_quote(private_key_path)
            ),
        )
        .env("GIT_TERMINAL_PROMPT", "0");
}

pub fn canonical_remote_ref_state(output: &[u8]) -> String {
    let mut lines = String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    lines.join("\n")
}

fn shell_quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("'{}'", path.replace('\'', "'\\''"))
}

struct KnownHostsHandler {
    host: String,
}

impl client::Handler for KnownHostsHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let known = keys::check_known_hosts(&self.host, 22, &server_public_key.public_key())
            .with_context(|| format!("failed to read SSH known_hosts for {}", self.host))?;
        if !known {
            anyhow::bail!(
                "SSH host key for {} is not present in the standard known_hosts file",
                self.host
            );
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SshConfig;
    use std::path::{Path, PathBuf};

    fn test_ssh_config(private_key_path: PathBuf) -> SshConfig {
        SshConfig {
            username: "backup".into(),
            host: "git.example.test".into(),
            private_key_path,
            root_dir: "/srv/git".into(),
            s3_path_prefix: "ssh/".into(),
        }
    }

    #[test]
    fn remote_repo_path_joins_root_and_child_without_duplicate_slashes() {
        assert_eq!(remote_repo_path("/srv/git/", "project"), "/srv/git/project");
        assert_eq!(remote_repo_path("/", "project"), "/project");
    }

    #[test]
    fn git_remote_url_does_not_include_private_key_or_password() {
        let config = test_ssh_config(PathBuf::from("/run/secrets/id_ed25519"));

        let url = git_remote_url(&config, "project");

        assert_eq!(url, "backup@git.example.test:/srv/git/project");
        assert!(!url.contains("id_ed25519"));
        assert_eq!(url.matches('@').count(), 1);
    }

    #[test]
    fn ssh_git_command_quotes_identity_path() {
        let mut command = tokio::process::Command::new("git");
        configure_git_ssh(&mut command, Path::new("/run/secrets/key with 'quote'"));

        let value = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == "GIT_SSH_COMMAND")
            .and_then(|(_, value)| value)
            .unwrap()
            .to_string_lossy();

        assert!(value.contains("IdentitiesOnly=yes"));
        assert!(value.contains("BatchMode=yes"));
        assert!(value.contains("key with"));
    }

    #[test]
    fn canonical_remote_ref_state_sorts_refs_and_omits_trailing_blank_lines() {
        let state = canonical_remote_ref_state(b"b-hash\trefs/heads/z\n a-hash\trefs/heads/a\n\n");

        assert_eq!(state, "a-hash\trefs/heads/a\nb-hash\trefs/heads/z");
    }
}
