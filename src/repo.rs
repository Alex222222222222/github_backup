use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::Context;
use futures_util::{AsyncWriteExt, StreamExt};
use log::{debug, error, info};
use opendal::Operator;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::{
    config::{CONFIG, SshConfig},
    ssh,
};

const CHUNK_SIZE: usize = 8 * 1024 * 1024;
const ARCHIVE_SUFFIX: &str = ".7z";
const GPG_ARCHIVE_SUFFIX: &str = ".7z.gpg";
const LEGACY_ARCHIVE_SUFFIX: &str = ".tar.zst";
const SSH_STATE_SUFFIX: &str = ".state";
const GIT_TOKEN_ENV: &str = "GITHUB_BACKUP_GIT_TOKEN";
const GIT_CREDENTIAL_HELPER_CONFIG: &str = "credential.helper=!f() { printf 'username=x-access-token\\npassword=%s\\n' \"$GITHUB_BACKUP_GIT_TOKEN\"; }; f";

pub enum RepoSource {
    Github {
        url: String,
        token: String,
        s3_path_prefix: String,
    },
    Ssh {
        url: String,
        port: u16,
        disable_host_key_check: bool,
        private_key_path: PathBuf,
        s3_path_prefix: String,
    },
}

pub struct Repo {
    pub name: String,
    pub updated_at: Option<i64>,
    pub archive_date: Option<i64>,
    pub source: RepoSource,
    pub remote_state: Option<String>,
    pub archived_state: Option<String>,
}

pub struct SynchronizationResult {
    pub remote_state: Option<String>,
    pub remote_updated_at: Option<i64>,
}

impl Repo {
    fn url(&self) -> String {
        match &self.source {
            RepoSource::Github { url, .. } | RepoSource::Ssh { url, .. } => url.clone(),
        }
    }

    fn s3_path_prefix(&self) -> &str {
        match &self.source {
            RepoSource::Github { s3_path_prefix, .. } | RepoSource::Ssh { s3_path_prefix, .. } => {
                s3_path_prefix
            }
        }
    }

    fn namespace(&self) -> &'static str {
        match &self.source {
            RepoSource::Github { .. } => "github",
            RepoSource::Ssh { .. } => "ssh",
        }
    }

    fn is_ssh(&self) -> bool {
        matches!(&self.source, RepoSource::Ssh { .. })
    }

    pub fn needs_backup(&self) -> bool {
        match &self.source {
            RepoSource::Github { .. } => {
                self.archive_date.is_none()
                    || self
                        .updated_at
                        .zip(self.archive_date)
                        .map(|(updated_at, archive_date)| archive_date < updated_at)
                        .unwrap_or(true)
            }
            RepoSource::Ssh { .. } => {
                self.archive_date.is_none()
                    || self.remote_state.is_none()
                    || self.remote_state != self.archived_state
                    || self.updated_at.is_none()
                    || timestamp_is_newer(self.updated_at, self.archive_date)
            }
        }
    }

    pub fn should_archive_after_sync(&self, remote_updated_at: Option<i64>) -> bool {
        if !self.is_ssh() {
            return true;
        }

        self.archive_date.is_none()
            || self.remote_state != self.archived_state
            || timestamp_is_newer(remote_updated_at, self.archive_date)
    }
}

fn timestamp_is_newer(updated_at: Option<i64>, archive_date: Option<i64>) -> bool {
    matches!((updated_at, archive_date), (Some(updated_at), Some(archive_date)) if updated_at > archive_date)
}

fn github_repo_url(username: &str, repo_name: &str) -> String {
    format!("https://github.com/{username}/{repo_name}.git")
}

fn configure_git_auth(command: &mut tokio::process::Command, token: &str) {
    command
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg(GIT_CREDENTIAL_HELPER_CONFIG)
        .env(GIT_TOKEN_ENV, token)
        .env("GIT_TERMINAL_PROMPT", "0");
}

pub async fn get_all_repos(object_store: &Operator) -> anyhow::Result<Vec<Repo>> {
    let mut repos = get_all_github_repos(object_store).await?;
    repos.extend(get_all_ssh_repos(object_store).await?);
    Ok(repos)
}

async fn get_all_github_repos(object_store: &Operator) -> anyhow::Result<Vec<Repo>> {
    let Some(github) = CONFIG.github.as_ref() else {
        return Ok(Vec::new());
    };

    info!(
        "Discovering GitHub repositories for user {}",
        github.username
    );
    #[derive(serde::Deserialize)]
    struct RepoRaw {
        pub name: String,
        pub updated_at: String,
    }

    /*
    * repos=$(curl -s \
      -H "Authorization: token $TOKEN" \
      "https://api.github.com/user/repos?affiliation=owner&per_page=$PER_PAGE&page=$page" 2>/dev/null)
    */
    let client = reqwest::Client::new();
    let mut page = 1;
    let mut repos = Vec::new();

    loop {
        let url = format!(
            "https://api.github.com/user/repos?affiliation=owner&per_page={}&page={}",
            CONFIG.per_page, page
        );
        let response = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "Alex222222222222")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", github.token),
            )
            .send()
            .await?;

        if !response.status().is_success() {
            debug!(
                "Failed to fetch repos from page {}: HTTP {}",
                page,
                response.status()
            );
            break;
        }

        let mut page_repos: Vec<RepoRaw> = response.json().await?;
        if page_repos.is_empty() {
            debug!("No more repos found on page {}, stopping fetch", page);
            break;
        }

        debug!("Fetched {} repos from page {}", page_repos.len(), page);

        repos.append(&mut page_repos);
        page += 1;
    }

    const MIN_UTC: chrono::DateTime<chrono::Utc> = chrono::DateTime::<chrono::Utc>::MIN_UTC;
    let mut all_archive_dates =
        get_all_repo_archive_dates(object_store, &github.s3_path_prefix).await?;

    info!(
        "Discovered {} GitHub repositories, {} with existing archives",
        repos.len(),
        all_archive_dates.len()
    );

    Ok(repos
        .into_iter()
        .map(|repo| {
            let name = repo.name;
            Repo {
                archive_date: all_archive_dates.remove(&name).flatten(),
                name: name.clone(),
                updated_at: Some(
                    chrono::DateTime::parse_from_rfc3339(&repo.updated_at)
                        .map_or(MIN_UTC.timestamp(), |dt| {
                            dt.with_timezone(&chrono::Utc).timestamp()
                        }),
                ),
                source: RepoSource::Github {
                    url: github_repo_url(&github.username, &name),
                    token: github.token.clone(),
                    s3_path_prefix: github.s3_path_prefix.clone(),
                },
                remote_state: None,
                archived_state: None,
            }
        })
        .collect())
}

async fn get_all_ssh_repos(object_store: &Operator) -> anyhow::Result<Vec<Repo>> {
    let Some(ssh_config) = CONFIG.ssh.as_ref() else {
        return Ok(Vec::new());
    };

    let explicit_repositories = ssh_config.repositories.is_some();
    if explicit_repositories {
        info!("Using explicit SSH repository list; skipping SFTP discovery");
    } else {
        info!(
            "Discovering SSH repositories under {} through SFTP",
            ssh_config.root_dir
        );
    }
    let repository_paths = match ssh_config.repositories.as_ref() {
        Some(repositories) => repositories.clone(),
        None => ssh::list_remote_directories(ssh_config).await?,
    };
    let archive_dates =
        get_all_repo_archive_dates(object_store, &ssh_config.s3_path_prefix).await?;
    let archived_states = get_all_repo_states(object_store, &ssh_config.s3_path_prefix).await?;
    let mut repos = Vec::new();
    let mut names = HashSet::new();

    for repository_path in repository_paths {
        let name = ssh::repository_name(&repository_path)
            .with_context(|| format!("invalid SSH repository path {repository_path:?}"))?;
        if !names.insert(name.clone()) {
            anyhow::bail!("SSH repository list contains duplicate name {name}");
        }

        let url = ssh::git_remote_url(ssh_config, &repository_path);
        let Some(remote_state) = probe_ssh_repository(ssh_config, &name, &url).await? else {
            if explicit_repositories {
                anyhow::bail!(
                    "Configured SSH repository {} is not a Git repository",
                    repository_path
                );
            }
            continue;
        };
        let updated_at = local_remote_updated_at_if_matching(&name, &remote_state).await?;

        repos.push(Repo {
            name: name.clone(),
            updated_at,
            archive_date: archive_dates.get(&name).copied().flatten(),
            source: RepoSource::Ssh {
                url,
                port: ssh_config.port,
                disable_host_key_check: ssh_config.disable_host_key_check,
                private_key_path: ssh_config.private_key_path.clone(),
                s3_path_prefix: ssh_config.s3_path_prefix.clone(),
            },
            archived_state: archived_states.get(&name).cloned(),
            remote_state: Some(remote_state),
        });
    }

    info!(
        "Discovered {} SSH repositories under {}",
        repos.len(),
        ssh_config.root_dir
    );
    Ok(repos)
}

async fn probe_ssh_repository(
    config: &SshConfig,
    name: &str,
    url: &str,
) -> anyhow::Result<Option<String>> {
    let mut command = tokio::process::Command::new("git");
    ssh::configure_git_ssh(
        &mut command,
        &config.private_key_path,
        config.port,
        config.disable_host_key_check,
    );
    let output = command
        .arg("ls-remote")
        .arg("--refs")
        .arg("--")
        .arg(url)
        .output()
        .await?;
    if output.status.success() {
        return Ok(Some(ssh::canonical_remote_ref_state(&output.stdout)));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_non_repository_probe_error(&stderr) {
        debug!("Skipping non-Git SSH directory {}: {}", name, stderr.trim());
        return Ok(None);
    }

    anyhow::bail!(
        "Failed to inspect SSH repository {}: {}",
        name,
        stderr.trim()
    )
}

async fn get_all_repo_archive_dates(
    object_store: &Operator,
    s3_path_prefix: &str,
) -> anyhow::Result<HashMap<String, Option<i64>>> {
    // list all archived repos in the S3 prefix; names may use the current or legacy suffix
    let mut archive_dates = HashMap::new();
    let mut lister = object_store.lister(s3_path_prefix).await?;
    while let Some(object) = lister.next().await {
        let object = match object {
            Ok(obj) => obj,
            Err(e) => {
                error!("Failed to list object: {}", e);
                continue;
            }
        };
        let key = object.name();
        if let Some(repo_name) = archive_repo_name_from_object_key(key) {
            record_archive_date(
                &mut archive_dates,
                repo_name,
                object
                    .metadata()
                    .last_modified()
                    .map(|t| t.into_inner().as_second()),
            );
        }
    }
    Ok(archive_dates)
}

async fn get_all_repo_states(
    object_store: &Operator,
    s3_path_prefix: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let mut states = HashMap::new();
    let mut lister = object_store.lister(s3_path_prefix).await?;
    while let Some(object) = lister.next().await {
        let object = match object {
            Ok(obj) => obj,
            Err(error) => {
                error!("Failed to list SSH state object: {}", error);
                continue;
            }
        };
        let key = object.name();
        let Some(name) = state_repo_name_from_object_key(key) else {
            continue;
        };
        let state_key = source_object_key(s3_path_prefix, name, SSH_STATE_SUFFIX);
        match object_store.read(&state_key).await {
            Ok(state) => {
                states.insert(
                    name.to_string(),
                    String::from_utf8_lossy(state.to_vec().as_ref())
                        .trim()
                        .to_string(),
                );
            }
            Err(error) => {
                error!("Failed to read SSH state object {}: {}", state_key, error);
            }
        }
    }
    Ok(states)
}

pub async fn clone_repo(repo: &Repo) -> anyhow::Result<SynchronizationResult> {
    let clone_dir = clone_dir_for(repo);
    tokio::fs::create_dir_all(&clone_dir).await?;

    let repo_dir = clone_repo_path_for(repo.namespace(), &repo.name);
    if repo_dir.exists() {
        info!("Updating existing mirror for repo {}", repo.name);
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .arg("remote")
            .arg("set-url")
            .arg("origin")
            .arg(repo.url())
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to sanitize remote URL for repo {}: {}",
                repo.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let mut command = authenticated_git_command(repo);
        let output = command
            .arg("-C")
            .arg(&repo_dir)
            .arg("remote")
            .arg("update")
            .arg("--prune")
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to update remote for repo {}: {}",
                repo.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    } else {
        info!("Cloning mirror for repo {}", repo.name);
        let mut command = authenticated_git_command(repo);
        let output = command
            .arg("clone")
            .arg("--mirror")
            .arg(repo.url())
            .arg(&repo_dir)
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "Failed to clone repo {}: {}",
                repo.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // gc the repo to reduce the size
    let output = tokio::process::Command::new("git")
        .arg("gc")
        .arg("--aggressive")
        .arg("--prune=now")
        .current_dir(&repo_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to gc repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // repack the repo to reduce the size
    let output = tokio::process::Command::new("git")
        .arg("repack")
        .arg("-a")
        .arg("-d")
        .arg("--window=250")
        .arg("--depth=250")
        .current_dir(&repo_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to repack repo {}: {}",
            repo.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let (remote_state, remote_updated_at) = if repo.is_ssh() {
        let metadata = local_repo_metadata(&repo_dir)
            .await?
            .ok_or_else(|| anyhow::anyhow!("synchronized repository {} is missing", repo.name))?;
        (Some(metadata.state), metadata.updated_at)
    } else {
        (None, None)
    };

    Ok(SynchronizationResult {
        remote_state,
        remote_updated_at,
    })
}

pub async fn archive_repo(repo: &Repo) -> anyhow::Result<()> {
    let archive_dir = archive_dir_for(repo);
    let clone_dir = clone_dir_for(repo);

    let gpg_key_files = configured_gpg_key_files()?;
    info!(
        "Creating 7z archive for repo {} (password encryption: {}, GPG encryption: {})",
        repo.name,
        !CONFIG.backup_password.is_empty(),
        gpg_key_files.is_some()
    );
    archive_repo_at(
        &archive_dir,
        &clone_dir,
        &repo.name,
        &CONFIG.backup_password,
    )
    .await?;

    if let Some(gpg_key_files) = gpg_key_files {
        info!(
            "Applying GPG encryption to repo {} archive with {} public key file(s)",
            repo.name,
            gpg_key_files.len()
        );
        let archive_path = archive_dir.join(format!("{}{}", repo.name, ARCHIVE_SUFFIX));
        encrypt_archive_with_gpg(&archive_path, &gpg_key_files).await?;
    }

    Ok(())
}

pub async fn upload_archive(object_store: &Operator, repo: &Repo) -> anyhow::Result<()> {
    let archive_suffix = current_archive_suffix();
    let archive_path = archive_dir_for(repo).join(format!("{}{}", repo.name, archive_suffix));
    let archive_upload_path = archive_object_key_with_suffix(repo, archive_suffix);
    info!(
        "{}",
        upload_log_message("archive", repo, &archive_upload_path)
    );
    upload_muiltipart(object_store, &archive_path, &archive_upload_path).await
}

pub async fn upload_state(object_store: &Operator, repo: &Repo, state: &str) -> anyhow::Result<()> {
    if !repo.is_ssh() {
        return Ok(());
    }

    let state_upload_path = source_object_key(repo.s3_path_prefix(), &repo.name, SSH_STATE_SUFFIX);
    info!("{}", upload_log_message("state", repo, &state_upload_path));
    object_store
        .write(&state_upload_path, state.as_bytes().to_vec())
        .await?;
    Ok(())
}

fn authenticated_git_command(repo: &Repo) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    match &repo.source {
        RepoSource::Github { token, .. } => configure_git_auth(&mut command, token),
        RepoSource::Ssh {
            port,
            disable_host_key_check,
            private_key_path,
            ..
        } => ssh::configure_git_ssh(
            &mut command,
            private_key_path,
            *port,
            *disable_host_key_check,
        ),
    }
    command
}

struct LocalRepoMetadata {
    state: String,
    updated_at: Option<i64>,
}

async fn local_remote_updated_at_if_matching(
    repo_name: &str,
    remote_state: &str,
) -> anyhow::Result<Option<i64>> {
    let repo_dir = clone_repo_path_for("ssh", repo_name);
    let Some(metadata) = local_repo_metadata(&repo_dir).await? else {
        return Ok(None);
    };

    if metadata.state == remote_state {
        Ok(metadata.updated_at)
    } else {
        Ok(None)
    }
}

async fn local_repo_metadata(repo_dir: &Path) -> anyhow::Result<Option<LocalRepoMetadata>> {
    if !repo_dir.is_dir() {
        return Ok(None);
    }

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("for-each-ref")
        .arg("--format=%(objectname)\t%(refname)\t%(creatordate:unix)")
        .arg("refs")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to read synchronized refs from {}: {}",
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(Some(parse_local_repo_metadata(&output.stdout)))
}

fn parse_local_repo_metadata(output: &[u8]) -> LocalRepoMetadata {
    let mut state_lines = Vec::new();
    for line in String::from_utf8_lossy(output).lines() {
        let mut fields = line.splitn(3, '\t');
        let Some(object_name) = fields.next() else {
            continue;
        };
        let Some(ref_name) = fields.next() else {
            continue;
        };
        if !object_name.is_empty() && !ref_name.is_empty() {
            state_lines.push(format!("{object_name}\t{ref_name}"));
        }
    }
    state_lines.sort();

    LocalRepoMetadata {
        state: state_lines.join("\n"),
        updated_at: latest_ref_update_timestamp(output),
    }
}

fn latest_ref_update_timestamp(output: &[u8]) -> Option<i64> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.rsplit_once('\t')?.1.parse::<i64>().ok())
        .max()
}

fn clone_root_for(namespace: &str) -> PathBuf {
    Path::new(&CONFIG.work_dir).join("clone").join(namespace)
}

fn clone_repo_path_for(namespace: &str, repo_name: &str) -> PathBuf {
    clone_root_for(namespace).join(format!("{repo_name}.git"))
}

fn clone_dir_for(repo: &Repo) -> PathBuf {
    clone_root_for(repo.namespace())
}

fn archive_dir_for(repo: &Repo) -> PathBuf {
    Path::new(&CONFIG.work_dir)
        .join("archive")
        .join(repo.namespace())
}

#[cfg(test)]
fn clone_path_for(repo: &Repo) -> PathBuf {
    PathBuf::from(repo.namespace()).join(format!("{}.git", repo.name))
}

#[cfg(test)]
fn archive_path_for(repo: &Repo) -> PathBuf {
    PathBuf::from(repo.namespace()).join(format!("{}{}", repo.name, ARCHIVE_SUFFIX))
}

#[cfg(test)]
fn archive_object_key(repo: &Repo) -> String {
    archive_object_key_with_suffix(repo, ARCHIVE_SUFFIX)
}

fn archive_object_key_with_suffix(repo: &Repo, suffix: &str) -> String {
    source_object_key(repo.s3_path_prefix(), &repo.name, suffix)
}

fn source_object_key(prefix: &str, repo_name: &str, suffix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{repo_name}{suffix}")
    } else {
        format!("{prefix}/{repo_name}{suffix}")
    }
}

fn upload_log_message(kind: &str, repo: &Repo, target: &str) -> String {
    format!(
        "Uploading {kind} for repo {} to S3 object {target}",
        repo.name
    )
}

fn archive_repo_name_from_object_key(key: &str) -> Option<&str> {
    let key = key.trim_matches('/').rsplit('/').next()?;
    key.strip_suffix(GPG_ARCHIVE_SUFFIX)
        .or_else(|| key.strip_suffix(ARCHIVE_SUFFIX))
        .or_else(|| key.strip_suffix(LEGACY_ARCHIVE_SUFFIX))
}

fn state_repo_name_from_object_key(key: &str) -> Option<&str> {
    key.trim_matches('/')
        .rsplit('/')
        .next()?
        .strip_suffix(SSH_STATE_SUFFIX)
}

fn is_non_repository_probe_error(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("does not appear to be a git repository")
        || stderr.contains("not a git repository")
        || stderr.contains("repository not found")
}

fn current_archive_suffix() -> &'static str {
    if CONFIG.gpg_public_key_dir.is_some() {
        GPG_ARCHIVE_SUFFIX
    } else {
        ARCHIVE_SUFFIX
    }
}

fn configured_gpg_key_files() -> anyhow::Result<Option<Vec<PathBuf>>> {
    let Some(directory) = CONFIG.gpg_public_key_dir.as_deref() else {
        return Ok(None);
    };

    Ok(Some(discover_gpg_key_files(std::path::Path::new(
        directory,
    ))?))
}

fn discover_gpg_key_files(directory: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut key_files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_file() {
            key_files.push(path);
        }
    }
    key_files.sort();

    if key_files.is_empty() {
        anyhow::bail!(
            "GPG_PUBLIC_KEY_DIR {} does not contain any regular key files",
            directory.display()
        );
    }

    Ok(key_files)
}

fn record_archive_date(
    archive_dates: &mut HashMap<String, Option<i64>>,
    repo_name: &str,
    archive_date: Option<i64>,
) {
    archive_dates
        .entry(repo_name.to_string())
        .and_modify(|current| {
            if archive_date > *current {
                *current = archive_date;
            }
        })
        .or_insert(archive_date);
}

async fn archive_repo_at(
    archive_dir: &std::path::Path,
    clone_dir: &std::path::Path,
    repo_name: &str,
    password: &str,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(archive_dir).await?;

    let archive_path =
        absolute_path(&archive_dir.join(format!("{}{}", repo_name, ARCHIVE_SUFFIX)))?;
    let temporary_archive_path =
        absolute_path(&archive_dir.join(format!("{}{}.part", repo_name, ARCHIVE_SUFFIX)))?;
    let clone_dir = absolute_path(clone_dir)?;
    let _ = tokio::fs::remove_file(&temporary_archive_path).await;

    let mut command = tokio::process::Command::new("7zz");
    command.arg("a").arg("-t7z").arg("-mx=1").arg("-y");
    if !password.is_empty() {
        command.arg("-mhe=on").arg(format!("-p{}", password));
    }
    let output = command
        .arg(&temporary_archive_path)
        .arg(format!("{}.git", repo_name))
        .current_dir(&clone_dir)
        .output()
        .await;

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary_archive_path).await;
            return Err(error.into());
        }
    };

    if !output.status.success() {
        let _ = tokio::fs::remove_file(&temporary_archive_path).await;
        anyhow::bail!(
            "Failed to archive repo {}: {}",
            repo_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tokio::fs::rename(temporary_archive_path, archive_path).await?;
    Ok(())
}

fn gpg_home_for_archive(archive_path: &std::path::Path) -> PathBuf {
    archive_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".gpg-home")
}

async fn ensure_gpg_home(gpg_home: &std::path::Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(gpg_home).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::set_permissions(gpg_home, std::fs::Permissions::from_mode(0o700)).await?;
    }

    Ok(())
}

async fn encrypt_archive_with_gpg(
    archive_path: &std::path::Path,
    key_files: &[PathBuf],
) -> anyhow::Result<()> {
    if key_files.is_empty() {
        anyhow::bail!("at least one GPG public key file is required");
    }

    let archive_path = absolute_path(archive_path)?;
    let gpg_home = gpg_home_for_archive(&archive_path);
    if let Err(error) = ensure_gpg_home(&gpg_home).await {
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(error);
    }
    let encrypted_archive_path = archive_path.with_file_name(format!(
        "{}{}",
        archive_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("archive path has no valid file name"))?,
        ".gpg"
    ));
    let temporary_encrypted_archive_path =
        PathBuf::from(format!("{}.part", encrypted_archive_path.display()));
    let _ = tokio::fs::remove_file(&temporary_encrypted_archive_path).await;

    let mut command = tokio::process::Command::new("gpg");
    command
        .arg("--no-options")
        .arg("--homedir")
        .arg(&gpg_home)
        .arg("--no-default-keyring")
        .arg("--batch")
        .arg("--yes")
        .arg("--no-tty")
        .arg("--encrypt");
    for key_file in key_files {
        command
            .arg("--recipient-file")
            .arg(absolute_path(key_file)?);
    }
    let output = command
        .arg("--output")
        .arg(&temporary_encrypted_archive_path)
        .arg(&archive_path)
        .output()
        .await;

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary_encrypted_archive_path).await;
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(error.into());
        }
    };

    if !output.status.success() {
        let _ = tokio::fs::remove_file(&temporary_encrypted_archive_path).await;
        let _ = tokio::fs::remove_file(&archive_path).await;
        anyhow::bail!(
            "Failed to encrypt archive with GPG: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tokio::fs::rename(temporary_encrypted_archive_path, encrypted_archive_path).await?;
    tokio::fs::remove_file(archive_path).await?;
    Ok(())
}

fn absolute_path(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

pub async fn upload_muiltipart(
    object_store: &Operator,
    origin: &PathBuf,
    target: &str,
) -> anyhow::Result<()> {
    let mut writer = object_store
        .writer_with(target)
        .chunk(CHUNK_SIZE)
        .concurrent(4)
        .await?
        .into_futures_async_write();
    let mut file = tokio::fs::File::open(origin).await?.compat();
    futures_util::io::copy(&mut file, &mut writer).await?;
    writer.close().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command as StdCommand, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn github_repo_url_does_not_include_credentials() {
        let url = github_repo_url("example-user", "example");

        assert_eq!(url, "https://github.com/example-user/example.git");
        assert!(!url.contains('@'));
    }

    fn test_github_repo(name: &str, s3_path_prefix: &str) -> Repo {
        Repo {
            name: name.to_string(),
            updated_at: Some(100),
            archive_date: None,
            source: RepoSource::Github {
                url: github_repo_url("example-user", name),
                token: "synthetic-token".into(),
                s3_path_prefix: s3_path_prefix.into(),
            },
            remote_state: None,
            archived_state: None,
        }
    }

    fn test_ssh_repo(name: &str, s3_path_prefix: &str) -> Repo {
        Repo {
            name: name.to_string(),
            updated_at: None,
            archive_date: None,
            source: RepoSource::Ssh {
                url: format!("backup@git.example.test:/srv/git/{name}"),
                port: 2222,
                disable_host_key_check: false,
                private_key_path: "/run/secrets/id_ed25519".into(),
                s3_path_prefix: s3_path_prefix.into(),
            },
            remote_state: None,
            archived_state: None,
        }
    }

    #[test]
    fn same_name_sources_have_distinct_local_paths_and_prefixes() {
        let github = test_github_repo("project", "github/");
        let ssh = test_ssh_repo("project", "ssh/");

        assert_ne!(clone_path_for(&github), clone_path_for(&ssh));
        assert_ne!(archive_path_for(&github), archive_path_for(&ssh));
        assert_eq!(archive_object_key(&github), "github/project.7z");
        assert_eq!(archive_object_key(&ssh), "ssh/project.7z");
    }

    #[test]
    fn upload_log_message_names_the_repo_and_s3_object() {
        let repo = test_ssh_repo("project", "ssh/");

        assert_eq!(
            upload_log_message("archive", &repo, "ssh/project.7z"),
            "Uploading archive for repo project to S3 object ssh/project.7z"
        );
    }

    #[test]
    fn archive_repo_name_accepts_legacy_and_current_suffixes_under_a_prefix() {
        assert_eq!(
            archive_repo_name_from_object_key("ssh/project.tar.zst"),
            Some("project")
        );
        assert_eq!(
            archive_repo_name_from_object_key("ssh/project.7z.gpg"),
            Some("project")
        );
        assert_eq!(archive_repo_name_from_object_key("ssh/project.zip"), None);
    }

    #[test]
    fn non_repository_probe_is_skipped() {
        assert!(is_non_repository_probe_error(
            "fatal: '/srv/git/notes' does not appear to be a git repository"
        ));
    }

    #[test]
    fn unexpected_ssh_probe_failure_is_returned() {
        assert!(!is_non_repository_probe_error(
            "Permission denied (publickey)."
        ));
    }

    #[test]
    fn ssh_backup_decision_uses_ref_state_and_legacy_archive() {
        let mut repo = test_ssh_repo("project", "ssh/");
        repo.archive_date = Some(100);
        repo.updated_at = Some(90);
        repo.remote_state = Some("new-state".into());
        repo.archived_state = Some("new-state".into());
        assert!(!repo.needs_backup());

        repo.remote_state = Some("changed-state".into());
        assert!(repo.needs_backup());
    }

    #[test]
    fn ssh_backup_decision_uses_newest_local_ref_timestamp() {
        let mut repo = test_ssh_repo("project", "ssh/");
        repo.archive_date = Some(100);
        repo.remote_state = Some("same-state".into());
        repo.archived_state = Some("same-state".into());

        repo.updated_at = Some(99);
        assert!(!repo.needs_backup());

        repo.updated_at = Some(101);
        assert!(repo.needs_backup());
    }

    #[test]
    fn ssh_archive_is_required_for_any_ref_change() {
        let mut repo = test_ssh_repo("project", "ssh/");
        repo.archive_date = Some(100);
        repo.remote_state = Some("new-state".into());
        repo.archived_state = Some("old-state".into());

        assert!(repo.should_archive_after_sync(Some(50)));
    }

    #[test]
    fn ssh_archive_is_skipped_when_refs_and_timestamp_are_not_newer() {
        let mut repo = test_ssh_repo("project", "ssh/");
        repo.archive_date = Some(100);
        repo.remote_state = Some("same-state".into());
        repo.archived_state = Some("same-state".into());

        assert!(!repo.should_archive_after_sync(Some(100)));
        assert!(!repo.should_archive_after_sync(Some(50)));
    }

    #[test]
    fn latest_ref_update_timestamp_includes_branches_and_tags() {
        let refs = b"hash-main\trefs/heads/main\t100\nhash-feature\trefs/heads/feature\t250\nhash-tag\trefs/tags/v1\t300\n";

        assert_eq!(latest_ref_update_timestamp(refs), Some(300));
        assert_eq!(
            parse_local_repo_metadata(refs).state,
            "hash-feature\trefs/heads/feature\nhash-main\trefs/heads/main\nhash-tag\trefs/tags/v1"
        );
    }

    #[test]
    fn git_credential_helper_returns_the_token_without_persisting_it_in_the_url() {
        let mut child = StdCommand::new("git")
            .args([
                "-c",
                "credential.helper=",
                "-c",
                GIT_CREDENTIAL_HELPER_CONFIG,
                "credential",
                "fill",
            ])
            .env(GIT_TOKEN_ENV, "synthetic-token")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .unwrap();

        let output = child.wait_with_output().unwrap();
        let credentials = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success());
        assert!(credentials.contains("username=x-access-token"));
        assert!(credentials.contains("password=synthetic-token"));
    }

    #[test]
    fn archive_repo_name_accepts_seven_zip_and_legacy_tar_zstd_objects() {
        assert_eq!(
            archive_repo_name_from_object_key("example.7z"),
            Some("example")
        );
        assert_eq!(
            archive_repo_name_from_object_key("example.7z.gpg"),
            Some("example")
        );
        assert_eq!(
            archive_repo_name_from_object_key("example.tar.zst"),
            Some("example")
        );
    }

    #[test]
    fn archive_repo_name_ignores_objects_with_unknown_suffixes() {
        assert_eq!(
            archive_repo_name_from_object_key("github-backup/example.zip"),
            None
        );
    }

    #[test]
    fn archive_dates_keep_the_newest_object_when_formats_coexist() {
        let mut archive_dates = HashMap::new();
        record_archive_date(&mut archive_dates, "example", Some(10));
        record_archive_date(&mut archive_dates, "example", Some(20));

        assert_eq!(archive_dates.get("example"), Some(&Some(20)));
    }

    #[test]
    fn gpg_key_files_are_discovered_in_stable_order() {
        let root = std::env::temp_dir().join(format!(
            "github-backup-gpg-key-discovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("z.asc"), b"z").unwrap();
        std::fs::write(root.join("a.asc"), b"a").unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();

        let key_files = discover_gpg_key_files(&root).unwrap();

        assert_eq!(key_files, vec![root.join("a.asc"), root.join("z.asc")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gpg_home_is_kept_next_to_the_archive() {
        let archive_path = std::path::Path::new("/work/archive/example.7z");

        assert_eq!(
            gpg_home_for_archive(archive_path),
            PathBuf::from("/work/archive/.gpg-home")
        );
    }

    #[tokio::test]
    #[ignore = "requires the 7zz runtime dependency"]
    async fn archive_repo_at_creates_a_password_protected_archive() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "github-backup-archive-test-{}-{}",
            std::process::id(),
            unique
        ));
        let clone_dir = root.join("clone");
        let archive_dir = root.join("archive");
        let repo_dir = clone_dir.join("example.git");
        let archive_path = archive_dir.join("example.7z");
        let password = "test backup password";

        tokio::fs::create_dir_all(&repo_dir).await.unwrap();
        tokio::fs::write(repo_dir.join("marker.txt"), "backup test")
            .await
            .unwrap();

        archive_repo_at(&archive_dir, &clone_dir, "example", password)
            .await
            .unwrap();

        let listing_without_password = tokio::process::Command::new("7zz")
            .arg("l")
            .arg(&archive_path)
            .output()
            .await
            .unwrap();
        let listing_output = format!(
            "{}{}",
            String::from_utf8_lossy(&listing_without_password.stdout),
            String::from_utf8_lossy(&listing_without_password.stderr)
        );
        assert!(!listing_output.contains("marker.txt"));

        let wrong_password = tokio::process::Command::new("7zz")
            .arg("t")
            .arg("-pwrong password")
            .arg(&archive_path)
            .output()
            .await
            .unwrap();
        assert!(!wrong_password.status.success());

        let correct_password = tokio::process::Command::new("7zz")
            .arg("t")
            .arg(format!("-p{password}"))
            .arg(&archive_path)
            .output()
            .await
            .unwrap();
        assert!(
            correct_password.status.success(),
            "7zz failed to test archive: {}",
            String::from_utf8_lossy(&correct_password.stderr)
        );

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires the 7zz runtime dependency"]
    async fn archive_repo_at_creates_an_unencrypted_archive_for_an_empty_password() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "github-backup-unencrypted-test-{}-{}",
            std::process::id(),
            unique
        ));
        let clone_dir = root.join("clone");
        let archive_dir = root.join("archive");
        let repo_dir = clone_dir.join("example.git");
        let archive_path = archive_dir.join("example.7z");

        tokio::fs::create_dir_all(&repo_dir).await.unwrap();
        tokio::fs::write(repo_dir.join("marker.txt"), "backup test")
            .await
            .unwrap();

        archive_repo_at(&archive_dir, &clone_dir, "example", "")
            .await
            .unwrap();

        let test_output = tokio::process::Command::new("7zz")
            .arg("t")
            .arg(&archive_path)
            .output()
            .await
            .unwrap();
        assert!(
            test_output.status.success(),
            "7zz failed to test unencrypted archive: {}",
            String::from_utf8_lossy(&test_output.stderr)
        );

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires the 7zz and gpg runtime dependencies"]
    async fn layered_encryption_supports_multiple_public_key_files() {
        let temp_dir = if std::path::Path::new("/private/tmp").is_dir() {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let root = temp_dir.join(format!(
            "gbg-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first_home = root.join("first-home");
        let second_home = root.join("second-home");
        let first_key = root.join("first.asc");
        let second_key = root.join("second.asc");
        let clone_dir = root.join("clone");
        let archive_dir = root.join("archive");
        let repo_dir = clone_dir.join("example.git");
        let archive = archive_dir.join("example.7z");
        let encrypted_archive = archive_dir.join("example.7z.gpg");

        std::fs::create_dir_all(&first_home).unwrap();
        std::fs::create_dir_all(&second_home).unwrap();
        set_secure_permissions(&first_home);
        set_secure_permissions(&second_home);

        generate_gpg_key(&first_home, "First Test <first@example.com>");
        generate_gpg_key(&second_home, "Second Test <second@example.com>");
        export_gpg_key(&first_home, "First Test <first@example.com>", &first_key);
        export_gpg_key(
            &second_home,
            "Second Test <second@example.com>",
            &second_key,
        );
        tokio::fs::create_dir_all(&repo_dir).await.unwrap();
        tokio::fs::write(repo_dir.join("marker.txt"), "backup test")
            .await
            .unwrap();
        archive_repo_at(&archive_dir, &clone_dir, "example", "test backup password")
            .await
            .unwrap();

        encrypt_archive_with_gpg(&archive, &[first_key, second_key])
            .await
            .unwrap();

        assert!(encrypted_archive.exists());
        assert!(!archive.exists());
        assert!(archive_dir.join(".gpg-home").is_dir());

        for (home, output_path) in [
            (&first_home, root.join("first-decrypted.7z")),
            (&second_home, root.join("second-decrypted.7z")),
        ] {
            let output = gpg_command(home)
                .arg("--output")
                .arg(&output_path)
                .arg("--decrypt")
                .arg(&encrypted_archive)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "gpg failed to decrypt: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let correct_password = tokio::process::Command::new("7zz")
                .arg("t")
                .arg("-ptest backup password")
                .arg(&output_path)
                .output()
                .await
                .unwrap();
            assert!(
                correct_password.status.success(),
                "7zz failed to test the decrypted archive: {}",
                String::from_utf8_lossy(&correct_password.stderr)
            );
        }

        let wrong_password = tokio::process::Command::new("7zz")
            .arg("t")
            .arg("-pwrong password")
            .arg(root.join("first-decrypted.7z"))
            .output()
            .await
            .unwrap();
        assert!(!wrong_password.status.success());

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    fn gpg_command(home: &std::path::Path) -> StdCommand {
        let mut command = StdCommand::new("gpg");
        command
            .arg("--no-options")
            .arg("--batch")
            .arg("--homedir")
            .arg(home);
        command
    }

    fn generate_gpg_key(home: &std::path::Path, identity: &str) {
        let output = gpg_command(home)
            .arg("--pinentry-mode")
            .arg("loopback")
            .arg("--passphrase")
            .arg("")
            .arg("--quick-generate-key")
            .arg(identity)
            .arg("rsa2048")
            .arg("encrypt")
            .arg("1d")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "gpg key generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn export_gpg_key(home: &std::path::Path, identity: &str, output_path: &std::path::Path) {
        let output = gpg_command(home)
            .arg("--armor")
            .arg("--export")
            .arg("--output")
            .arg(output_path)
            .arg(identity)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "gpg public-key export failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn set_secure_permissions(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_secure_permissions(_path: &std::path::Path) {}
}
