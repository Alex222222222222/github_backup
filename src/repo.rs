use std::{collections::HashMap, path::PathBuf};

use futures_util::{AsyncWriteExt, StreamExt};
use log::{debug, error};
use opendal::Operator;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::config::CONFIG;

const CHUNK_SIZE: usize = 8 * 1024 * 1024;
const ARCHIVE_SUFFIX: &str = ".7z";
const GPG_ARCHIVE_SUFFIX: &str = ".7z.gpg";
const LEGACY_ARCHIVE_SUFFIX: &str = ".tar.zst";
const GIT_TOKEN_ENV: &str = "GITHUB_BACKUP_GIT_TOKEN";
const GIT_CREDENTIAL_HELPER_CONFIG: &str = "credential.helper=!f() { printf 'username=x-access-token\\npassword=%s\\n' \"$GITHUB_BACKUP_GIT_TOKEN\"; }; f";

pub struct Repo {
    pub name: String,
    pub updated_at: i64,
    pub archive_date: Option<i64>,
}
impl Repo {
    fn url(&self) -> String {
        github_repo_url(&CONFIG.github_username, &self.name)
    }
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
    debug!(
        "Starting to fetch all repos for user {}",
        CONFIG.github_username
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
                format!("token {}", CONFIG.github_token),
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
    let mut all_archive_dates = get_all_repo_archive_dates(object_store).await?;

    debug!(
        "Fetched {} repos, {} archived repos",
        repos.len(),
        all_archive_dates.len()
    );

    Ok(repos
        .into_iter()
        .map(|repo| Repo {
            archive_date: all_archive_dates.remove(&repo.name).flatten(),
            name: repo.name,
            updated_at: chrono::DateTime::parse_from_rfc3339(&repo.updated_at)
                .map_or(MIN_UTC.timestamp(), |dt| {
                    dt.with_timezone(&chrono::Utc).timestamp()
                }),
        })
        .collect())
}

async fn get_all_repo_archive_dates(
    object_store: &Operator,
) -> anyhow::Result<HashMap<String, Option<i64>>> {
    // list all archived repos in the S3 prefix; names may use the current or legacy suffix
    let mut archive_dates = HashMap::new();
    let mut lister = object_store.lister(&CONFIG.s3_path_prefix).await?;
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

pub async fn clone_repo(repo: &Repo) -> anyhow::Result<()> {
    // make sure work_dir/clone exists
    let clone_dir = std::path::Path::new(&CONFIG.work_dir).join("clone");
    tokio::fs::create_dir_all(&clone_dir).await?;

    // test if work_dir/clone/name.git already exists,
    // if exists,`git -C "work_dir/clone/name.git" remote update`
    let repo_dir = clone_dir.join(format!("{}.git", repo.name));
    if repo_dir.exists() {
        debug!("Repo {} already exists, updating remote", repo.name);
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

        let mut command = tokio::process::Command::new("git");
        configure_git_auth(&mut command, &CONFIG.github_token);
        let output = command
            .arg("-C")
            .arg(&repo_dir)
            .arg("remote")
            .arg("update")
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
        // use tokio::Command to run git clone --mirror repo.url
        let mut command = tokio::process::Command::new("git");
        configure_git_auth(&mut command, &CONFIG.github_token);
        let output = command
            .arg("clone")
            .arg("--mirror")
            .arg(repo.url())
            .current_dir(clone_dir)
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

    Ok(())
}

pub async fn archive_repo(repo: &Repo) -> anyhow::Result<()> {
    let archive_dir = std::path::Path::new(&CONFIG.work_dir).join("archive");
    let clone_dir = std::path::Path::new(&CONFIG.work_dir).join("clone");

    let gpg_key_files = configured_gpg_key_files()?;
    archive_repo_at(
        &archive_dir,
        &clone_dir,
        &repo.name,
        &CONFIG.backup_password,
    )
    .await?;

    if let Some(gpg_key_files) = gpg_key_files {
        let archive_path = archive_dir.join(format!("{}{}", repo.name, ARCHIVE_SUFFIX));
        encrypt_archive_with_gpg(&archive_path, &gpg_key_files).await?;
    }

    Ok(())
}

pub async fn upload_archive(object_store: &Operator, repo: &Repo) -> anyhow::Result<()> {
    let archive_suffix = current_archive_suffix();
    let archive_path = std::path::Path::new(&CONFIG.work_dir)
        .join("archive")
        .join(format!("{}{}", repo.name, archive_suffix));
    let archive_upload_path = &format!(
        "{}/{}{}",
        CONFIG.s3_path_prefix.trim_matches('/'),
        repo.name,
        archive_suffix
    );
    upload_muiltipart(object_store, &archive_path, archive_upload_path).await
}

fn archive_repo_name_from_object_key(key: &str) -> Option<&str> {
    let key = key.trim_matches('/');
    key.strip_suffix(GPG_ARCHIVE_SUFFIX)
        .or_else(|| key.strip_suffix(ARCHIVE_SUFFIX))
        .or_else(|| key.strip_suffix(LEGACY_ARCHIVE_SUFFIX))
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

async fn encrypt_archive_with_gpg(
    archive_path: &std::path::Path,
    key_files: &[PathBuf],
) -> anyhow::Result<()> {
    if key_files.is_empty() {
        anyhow::bail!("at least one GPG public key file is required");
    }

    let archive_path = absolute_path(archive_path)?;
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
