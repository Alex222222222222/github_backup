pub static CONFIG: once_cell::sync::Lazy<Config> = once_cell::sync::Lazy::new(|| {
    Config::from_env().expect("Failed to load configuration from environment variables")
});

use std::path::PathBuf;

pub struct GithubConfig {
    pub username: String,
    pub token: String,
    pub s3_path_prefix: String,
}

pub struct SshConfig {
    pub username: String,
    pub host: String,
    pub private_key_path: PathBuf,
    pub root_dir: String,
    pub s3_path_prefix: String,
}

pub struct Config {
    pub github: Option<GithubConfig>,
    pub ssh: Option<SshConfig>,
    pub backup_password: String,
    pub gpg_public_key_dir: Option<String>,
    pub per_page: usize,
    pub work_dir: String,

    pub s3_endpoint: String,
    pub s3_access_key_id: String,
    pub s3_access_key: String,
    pub s3_bucket_name: String,
    pub s3_virtual_hosted_style_request: bool,
    pub s3_region: Option<String>,
}
impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    fn from_env_with<F>(mut get: F) -> anyhow::Result<Self>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let github_username = non_empty_env(&mut get, "GITHUB_USERNAME");
        let github_token = non_empty_env(&mut get, "GITHUB_TOKEN");
        let github = match (github_username, github_token) {
            (None, None) => None,
            (Some(username), Some(token)) => Some(GithubConfig {
                username,
                token,
                s3_path_prefix: required_env(&mut get, "S3_PATH_PREFIX")?,
            }),
            (None, Some(_)) => {
                anyhow::bail!("GITHUB_USERNAME is required when GITHUB_TOKEN is configured")
            }
            (Some(_), None) => {
                anyhow::bail!("GITHUB_TOKEN is required when GITHUB_USERNAME is configured")
            }
        };

        let ssh_username = non_empty_env(&mut get, "SSH_USERNAME");
        let ssh_host = non_empty_env(&mut get, "SSH_HOST");
        let ssh_private_key_path = non_empty_env(&mut get, "SSH_PRIVATE_KEY_PATH");
        let ssh_root_dir = non_empty_env(&mut get, "SSH_ROOT_DIR");
        let ssh_s3_path_prefix = non_empty_env(&mut get, "SSH_S3_PATH_PREFIX");
        let ssh_values = [
            ("SSH_USERNAME", ssh_username.is_some()),
            ("SSH_HOST", ssh_host.is_some()),
            ("SSH_PRIVATE_KEY_PATH", ssh_private_key_path.is_some()),
            ("SSH_ROOT_DIR", ssh_root_dir.is_some()),
            ("SSH_S3_PATH_PREFIX", ssh_s3_path_prefix.is_some()),
        ];
        let ssh = if ssh_values.iter().any(|(_, configured)| *configured) {
            let missing = ssh_values
                .iter()
                .filter_map(|(name, configured)| (!configured).then_some(*name))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                anyhow::bail!(
                    "SSH source is partially configured; missing {}",
                    missing.join(", ")
                );
            }

            Some(SshConfig {
                username: ssh_username.expect("validated SSH_USERNAME"),
                host: ssh_host.expect("validated SSH_HOST"),
                private_key_path: PathBuf::from(
                    ssh_private_key_path.expect("validated SSH_PRIVATE_KEY_PATH"),
                ),
                root_dir: ssh_root_dir.expect("validated SSH_ROOT_DIR"),
                s3_path_prefix: ssh_s3_path_prefix.expect("validated SSH_S3_PATH_PREFIX"),
            })
        } else {
            None
        };

        if github.is_none() && ssh.is_none() {
            anyhow::bail!(
                "at least one backup source must be configured with GitHub or SSH settings"
            );
        }

        Ok(Self {
            github,
            ssh,
            backup_password: get("BACKUP_PASSWORD").unwrap_or_default(),
            gpg_public_key_dir: get("GPG_PUBLIC_KEY_DIR").filter(|path| !path.is_empty()),
            per_page: get("PER_PAGE")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100),
            work_dir: get("WORK_DIR").unwrap_or("./backup".to_string()),
            s3_endpoint: required_env(&mut get, "S3_ENDPOINT")?,
            s3_access_key_id: required_env(&mut get, "S3_ACCESS_KEY_ID")?,
            s3_access_key: required_env(&mut get, "S3_ACCESS_KEY")?,
            s3_bucket_name: required_env(&mut get, "S3_BUCKET_NAME")?,
            s3_virtual_hosted_style_request: get("S3_VIRTUAL_HOSTED_STYLE_REQUEST")
                .and_then(|s| s.parse::<bool>().ok())
                .unwrap_or(false),
            s3_region: get("S3_REGION").filter(|region| !region.is_empty()),
        })
    }
}

fn non_empty_env<F>(get: &mut F, name: &str) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    get(name).filter(|value| !value.is_empty())
}

fn required_env<F>(get: &mut F, name: &str) -> anyhow::Result<String>
where
    F: FnMut(&str) -> Option<String>,
{
    non_empty_env(get, name).ok_or_else(|| anyhow::anyhow!("{name} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_environment() -> HashMap<String, String> {
        [
            ("S3_ENDPOINT", "https://s3.example.test"),
            ("S3_ACCESS_KEY_ID", "access"),
            ("S3_ACCESS_KEY", "secret"),
            ("S3_BUCKET_NAME", "backup"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
    }

    fn github_environment() -> impl FnMut(&str) -> Option<String> {
        let mut environment = base_environment();
        environment.insert("GITHUB_USERNAME".into(), "github-user".into());
        environment.insert("GITHUB_TOKEN".into(), "github-token".into());
        environment.insert("S3_PATH_PREFIX".into(), "github/".into());
        move |name| environment.get(name).cloned()
    }

    fn ssh_environment() -> impl FnMut(&str) -> Option<String> {
        let mut environment = base_environment();
        environment.insert("SSH_USERNAME".into(), "backup".into());
        environment.insert("SSH_HOST".into(), "git.example.test".into());
        environment.insert(
            "SSH_PRIVATE_KEY_PATH".into(),
            "/run/secrets/id_ed25519".into(),
        );
        environment.insert("SSH_ROOT_DIR".into(), "/srv/git".into());
        environment.insert("SSH_S3_PATH_PREFIX".into(), "ssh/".into());
        move |name| environment.get(name).cloned()
    }

    fn both_sources_environment() -> impl FnMut(&str) -> Option<String> {
        let mut environment = base_environment();
        environment.insert("GITHUB_USERNAME".into(), "github-user".into());
        environment.insert("GITHUB_TOKEN".into(), "github-token".into());
        environment.insert("S3_PATH_PREFIX".into(), "github/".into());
        environment.insert("SSH_USERNAME".into(), "backup".into());
        environment.insert("SSH_HOST".into(), "git.example.test".into());
        environment.insert(
            "SSH_PRIVATE_KEY_PATH".into(),
            "/run/secrets/id_ed25519".into(),
        );
        environment.insert("SSH_ROOT_DIR".into(), "/srv/git".into());
        environment.insert("SSH_S3_PATH_PREFIX".into(), "ssh/".into());
        move |name| environment.get(name).cloned()
    }

    #[test]
    fn ssh_only_configuration_does_not_require_github_variables() {
        let config = Config::from_env_with(ssh_environment()).unwrap();

        assert!(config.github.is_none());
        assert_eq!(config.ssh.unwrap().root_dir, "/srv/git");
    }

    #[test]
    fn both_sources_require_distinct_s3_prefixes() {
        let config = Config::from_env_with(both_sources_environment()).unwrap();

        assert_eq!(config.github.unwrap().s3_path_prefix, "github/");
        assert_eq!(config.ssh.unwrap().s3_path_prefix, "ssh/");
    }

    #[test]
    fn partial_ssh_configuration_is_rejected() {
        let mut environment = base_environment();
        environment.insert("SSH_HOST".into(), "git.example.test".into());

        let error = match Config::from_env_with(|name| environment.get(name).cloned()) {
            Ok(_) => panic!("partial SSH configuration should be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("SSH_USERNAME"));
    }

    #[test]
    fn empty_backup_password_is_allowed() {
        let config = Config::from_env_with(github_environment()).unwrap();

        assert!(config.backup_password.is_empty());
    }
}
