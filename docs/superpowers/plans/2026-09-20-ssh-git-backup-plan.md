# Remote SSH Git Backup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add SFTP-based remote repository discovery and Git-over-SSH mirror synchronization while preserving the existing GitHub, archive, encryption, legacy-object, and failure-reporting behavior.

**Architecture:** Make `Config` represent optional GitHub and SSH sources, add an SSH transport module that uses SFTP only for directory enumeration and verifies host keys, and make `Repo` carry source-specific Git authentication, S3 prefix, local namespace, and SSH ref state. Reuse one mirror/archive/encryption/upload pipeline for both sources, with an SSH state sidecar to skip unchanged remote repositories across runs.

**Tech Stack:** Rust 2024, Tokio, `russh` 0.63 for authenticated SSH/SFTP transport, `russh-sftp` 3.0 for directory enumeration, Git CLI for Git-over-SSH mirror operations, OpenDAL S3, 7-Zip, GPG, and existing Rust unit tests.

**Spec:** `docs/superpowers/specs/2026-09-20-ssh-git-backup-design.md`

## Global Constraints

- Never read `.env`; tests must use synthetic values and local temporary resources only.
- The GitHub source is enabled only when `GITHUB_USERNAME` and `GITHUB_TOKEN` are both non-empty.
- The SSH source is enabled only when `SSH_USERNAME`, `SSH_HOST`, `SSH_PRIVATE_KEY_PATH`, `SSH_ROOT_DIR`, and `SSH_S3_PATH_PREFIX` are all non-empty.
- At least one source must be configured; source prefixes must remain independent.
- SFTP is used only to list immediate child directories; Git synchronization uses `git clone --mirror` and `git remote update --prune` over SSH.
- SSH credentials and GitHub tokens must not appear in Git remote URLs or persisted repository configuration.
- SSH host keys are verified against the process user's standard `~/.ssh/known_hosts`.
- Existing `.tar.zst`, `.7z`, and `.7z.gpg` archive objects remain recognized.
- Empty `BACKUP_PASSWORD` means no 7z password encryption.
- Every feature/behavior test must be written and observed failing before its production implementation is added.
- The program continues after per-repository failures and exits non-zero if any repository failed.

## Review Focus

- SSH-only configuration must start without GitHub variables and must not accidentally call the GitHub API; test: `ssh_only_configuration_does_not_require_github_variables` in Task 1.
- A same-named GitHub and SSH repository must not overwrite local clone/archive files or S3 objects; test: `same_name_sources_have_distinct_local_paths_and_prefixes` in Task 3.
- A directory that is not a repository must be skipped while an SSH authentication/transport failure must fail discovery; tests: `non_repository_probe_is_skipped` and `unexpected_ssh_probe_failure_is_returned` in Task 3.
- A changed SSH ref state must create a new backup, while an unchanged state with an existing legacy archive must be skipped; test: `ssh_backup_decision_uses_ref_state_and_legacy_archive` in Task 3.
- A private key path containing shell metacharacters must be quoted in `GIT_SSH_COMMAND` without being placed in the remote URL; test: `ssh_git_command_quotes_identity_path` in Task 2.

---

### Task 1: Optional source configuration

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs` unit test module
- Modify: `src/repo.rs` imports and GitHub configuration accesses as needed

**Interfaces:**
- Produces `GithubConfig { username: String, token: String, s3_path_prefix: String }` and `SshConfig { username: String, host: String, private_key_path: PathBuf, root_dir: String, s3_path_prefix: String }`.
- Produces `Config { github: Option<GithubConfig>, ssh: Option<SshConfig>, ... }` and `Config::from_env_with`, which parses a supplied environment lookup closure for deterministic tests.

- [ ] **Step 1: Write the failing tests**

Add configuration tests using a `HashMap<String, String>` lookup and synthetic S3 values:

```rust
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
    let mut env = base_environment();
    env.insert("SSH_HOST".into(), "git.example.test".into());

    let error = Config::from_env_with(|name| env.get(name).cloned()).unwrap_err();

    assert!(error.to_string().contains("SSH_USERNAME"));
}

#[test]
fn empty_backup_password_is_allowed() {
    let config = Config::from_env_with(github_environment()).unwrap();

    assert!(config.backup_password.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test config::tests -- --nocapture`

Expected: FAIL because `Config` has no optional source structs or `from_env_with` parser yet.

- [ ] **Step 3: Implement the minimal configuration model**

Add the two source structs, parse non-empty optional variables, validate source fields atomically, require at least one enabled source, and require `S3_PATH_PREFIX` only when GitHub is enabled. Keep all existing S3, encryption, work-directory, pagination, and logging parsing unchanged. Make `from_env()` delegate to `from_env_with` using `std::env::var(name).ok()`.

- [ ] **Step 4: Update existing GitHub references to the optional model**

Change GitHub code to read `CONFIG.github` only after the source is known to be enabled. Preserve the existing GitHub username/token values and credential-helper behavior.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test config::tests -- --nocapture`

Expected: PASS with all configuration tests green.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs src/repo.rs
git commit -m "feat: make backup sources independently configurable"
```

### Task 2: SSH/SFTP discovery and Git authentication

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock` (generated by Cargo)
- Create: `src/ssh.rs`
- Modify: `src/lib.rs`
- Test: `src/ssh.rs` unit test module
- Modify: `Dockerfile` to install the OpenSSH client used by Git

**Interfaces:**
- Consumes `config::SshConfig`.
- Produces `pub async fn list_remote_directories(config: &SshConfig) -> anyhow::Result<Vec<String>>`.
- Produces `pub fn remote_repo_path(root_dir: &str, repo_name: &str) -> String`.
- Produces `pub fn git_remote_url(config: &SshConfig, repo_name: &str) -> String`.
- Produces `pub fn configure_git_ssh(command: &mut tokio::process::Command, private_key_path: &Path)`.
- Produces `pub fn canonical_remote_ref_state(output: &[u8]) -> String`.

- [ ] **Step 1: Write the failing pure helper tests**

Add tests for path joining, credential-free URL construction, shell-safe key configuration, and stable ref-state normalization:

```rust
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

    let value = command.as_std().get_envs()
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ssh::tests -- --nocapture`

Expected: FAIL because `src::ssh` and its helper functions do not exist.

- [ ] **Step 3: Add the SFTP and SSH dependencies**

Add `russh = "0.63.3"` and `russh-sftp = "3.0.0"` to `Cargo.toml`, then run `cargo check` to generate the lockfile entries. Keep the existing Tokio features; the new crates contribute their required Tokio features.

- [ ] **Step 4: Implement SSH path, command, and state helpers**

Build SCP-style Git URLs from the configured remote path. Quote the identity path for the shell-valued `GIT_SSH_COMMAND`, adding `-i`, `IdentitiesOnly=yes`, and `BatchMode=yes`; do not put the key path into the Git URL. Normalize `git ls-remote` output by trimming empty lines, sorting lines, and joining them with `\n`.

- [ ] **Step 5: Implement host-key-checked SFTP directory listing**

Use `russh::client::connect` with a handler whose `check_server_key` calls `russh::keys::check_known_hosts` for the configured host and standard port 22. Load the configured private key, authenticate with public-key auth, open the `sftp` subsystem, read `SSH_ROOT_DIR`, retain directory entries, sort their names, close the SFTP/session handles, and return the names. Never open a shell channel or execute an arbitrary command.

- [ ] **Step 6: Run the focused tests and compile check**

Run: `cargo test ssh::tests -- --nocapture`

Expected: PASS.

Run: `cargo check`

Expected: PASS with the new SSH transport compiling.

- [ ] **Step 7: Update the runtime image**

Install `openssh-client` in the Alpine runner so Git's SSH transport is available, create `/home/app/.ssh` for the app user, and leave host-key verification to the mounted standard `known_hosts` file.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/ssh.rs src/lib.rs Dockerfile
git commit -m "feat: add SFTP SSH repository discovery"
```

### Task 3: Source-neutral repository collection, mirrors, prefixes, and SSH state

**Files:**
- Modify: `src/repo.rs`
- Test: `src/repo.rs` unit test module
- Modify: `src/ssh.rs` only if the repository probe needs a shared helper

**Interfaces:**
- Produces `RepoSource::Github` and `RepoSource::Ssh` variants carrying URL, authentication, S3 prefix, and source namespace.
- Produces `Repo { name, updated_at, archive_date, source, remote_state, archived_state }`.
- Produces `pub async fn get_all_repos(object_store: &Operator) -> anyhow::Result<Vec<Repo>>` that combines enabled GitHub and SSH sources.
- Changes `pub async fn clone_repo(repo: &Repo) -> anyhow::Result<Option<String>>` to return the ref state of the synchronized mirror when applicable.
- Keeps `archive_repo` and `upload_archive` as the shared archive pipeline, with source-specific local paths and S3 prefixes.

- [ ] **Step 1: Write the failing repository-model tests**

Add tests that pin source namespaces, archive parsing, state keys, and skip decisions:

```rust
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
fn archive_repo_name_accepts_legacy_and_current_suffixes_under_a_prefix() {
    assert_eq!(archive_repo_name_from_object_key("ssh/project.tar.zst"), Some("project"));
    assert_eq!(archive_repo_name_from_object_key("ssh/project.7z.gpg"), Some("project"));
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
    repo.remote_state = Some("new-state".into());
    repo.archived_state = Some("new-state".into());
    assert!(!repo.needs_backup());

    repo.remote_state = Some("changed-state".into());
    assert!(repo.needs_backup());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test repo::tests -- --nocapture`

Expected: FAIL because `RepoSource`, source-aware paths, SSH state, and `needs_backup` do not exist.

- [ ] **Step 3: Introduce the source-neutral repository types**

Replace the GitHub-only URL field logic with `RepoSource` variants. Keep GitHub's URL as `https://github.com/<user>/<repo>.git` and SSH's URL from `ssh::git_remote_url`. Give each source a stable namespace (`github` or `ssh`) and its configured S3 prefix. Use source-specific `work_dir/clone/<namespace>/` and `work_dir/archive/<namespace>/` directories.

- [ ] **Step 4: Make S3 archive discovery prefix-aware**

Pass a prefix into archive-date listing, parse only the object basename, and preserve the newest timestamp when `.tar.zst`, `.7z`, and `.7z.gpg` coexist. Add SSH state-object listing/reading for `<repo>.state` objects under the SSH prefix. Unknown suffixes remain ignored.

- [ ] **Step 5: Implement GitHub collection without requiring GitHub configuration**

Return an empty list when GitHub is disabled. Otherwise retain the current paginated API request, timestamp parsing, credential-free URL, and archive-date lookup under the GitHub prefix. Do not call the API for SSH-only configuration.

- [ ] **Step 6: Implement SSH discovery and repository probing**

Call `ssh::list_remote_directories`, construct one candidate URL per directory, and run `git ls-remote` with the SSH key. Normalize successful output into `remote_state`. Skip only errors that clearly identify a non-repository path; return unexpected/authentication/transport errors. Read any existing `<repo>.state` object and populate `archived_state`.

- [ ] **Step 7: Implement source-aware mirror synchronization**

For a missing source-specific mirror, run `git clone --mirror`. For an existing mirror, set its origin to the credential-free URL and run `git remote update --prune`, with either the GitHub credential helper or the SSH `GIT_SSH_COMMAND`. Run the existing garbage collection and repack steps. Return the normalized local mirror ref state after synchronization.

- [ ] **Step 8: Connect the SSH state to archive/upload behavior**

Use `Repo::needs_backup` for GitHub timestamp decisions and SSH archive/state comparisons. Upload the archive under the source prefix, then upload the local synchronized SSH ref state to the matching `.state` object. Preserve the existing optional 7z password and GPG layering and all cleanup/error handling.

- [ ] **Step 9: Run repository tests and the full suite**

Run: `cargo test repo::tests -- --nocapture`

Expected: PASS, with only runtime-dependent archive tests remaining ignored.

Run: `cargo test --all-targets`

Expected: PASS with no new failures.

- [ ] **Step 10: Commit**

```bash
git add src/repo.rs src/ssh.rs
git commit -m "feat: synchronize and archive SSH Git mirrors"
```

### Task 4: Main orchestration and documentation

**Files:**
- Modify: `src/main.rs`
- Modify: `README.md`
- Test: `src/main.rs` unit test module if orchestration helpers are extracted

**Interfaces:**
- Consumes the combined `repo::get_all_repos` list and source-aware `clone_repo`, `archive_repo`, and upload functions from Task 3.
- Preserves `finish_backup` as the final non-zero failure gate.

- [ ] **Step 1: Write the failing orchestration regression test**

Add a pure selection helper and test showing that an SSH repository with matching state is skipped. Use synthetic `Repo` values only; do not construct `CONFIG` or call S3:

```rust
#[test]
fn backup_selection_uses_source_aware_repo_state() {
    let repo = test_ssh_repo_with_states("project", Some("same"), Some("same"));

    assert!(!should_process_repo(&repo));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test main::tests -- --nocapture`

Expected: FAIL because `should_process_repo` has not been added yet.

- [ ] **Step 3: Run both sources through one failure-reporting loop**

Load the combined source list, skip repositories whose source-specific `needs_backup` is false, synchronize before archiving, archive and upload each selected repository, upload SSH state after its archive, continue after each failure, and pass all failed source/repository labels to `finish_backup`.

- [ ] **Step 4: Document SSH configuration and restore behavior**

Update `README.md` with all SSH variables, source-prefix examples, SFTP discovery plus Git-over-SSH semantics, standard `known_hosts` mounting, an SSH-only Docker invocation, a combined GitHub+SSH invocation pattern, `.state` sidecars, same-name isolation, and the existing 7z/GPG restore commands. Explicitly state that `BACKUP_PASSWORD` may be empty and that GitHub and SSH prefixes must be different when names may overlap.

- [ ] **Step 5: Run formatting, lint, and complete verification**

Run: `cargo fmt -- --check`

Expected: PASS.

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS.

Run: `cargo test --all-targets`

Expected: PASS; runtime-dependent archive/GPG tests may remain explicitly ignored.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs README.md
git commit -m "docs: configure remote SSH Git backups"
```

## Final verification

After all tasks, inspect the complete diff for accidental `.env` access, credentials in URL/config strings, unsafely accepted host keys, cross-source path collisions, and unhandled failures. Run `git status --short --branch` and report all commits and verification results before requesting any push or other shared-branch operation.
