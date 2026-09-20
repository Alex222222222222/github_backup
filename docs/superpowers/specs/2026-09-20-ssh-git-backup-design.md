# Remote SSH Git Backup Design

## Goal

Add a second backup source for repositories hosted under a remote SSH-accessible directory. The application must discover immediate child directories through SFTP, synchronize valid Git repositories through Git-over-SSH, and store their archives under a source-specific S3 prefix. GitHub and SSH sources must be usable simultaneously.

## User-facing configuration

The GitHub source is enabled when both `GITHUB_USERNAME` and `GITHUB_TOKEN` are non-empty. Its archive prefix remains `S3_PATH_PREFIX`.

The SSH source is enabled when all of these variables are non-empty:

- `SSH_USERNAME`
- `SSH_HOST`
- `SSH_PRIVATE_KEY_PATH`
- `SSH_ROOT_DIR`
- `SSH_S3_PATH_PREFIX`

`SSH_ROOT_DIR` is the remote directory whose immediate child directories are inspected. The private key is used for both SFTP discovery and Git-over-SSH. SSH host keys are verified against the process user's standard `~/.ssh/known_hosts`; the same file is used by the OpenSSH client invoked by Git.

The common S3 endpoint, credentials, bucket, and optional region/virtual-host settings remain unchanged. At least one source must be configured. GitHub-only deployments still require `S3_PATH_PREFIX`; SSH-only deployments require `SSH_S3_PATH_PREFIX` instead. A partial source configuration is an error with the missing source variables named in the message.

## Discovery and synchronization

1. Connect to `SSH_HOST:22` using the configured username and private key with an SFTP subsystem. Do not execute arbitrary remote shell commands for discovery.
2. List only the immediate children of `SSH_ROOT_DIR` and keep directory entries.
3. For each directory, construct a Git SSH URL from the configured user, host, root, and child name, then run `git ls-remote` with the configured key. A successful probe identifies a repository; a normal “not a Git repository” response skips that directory. Authentication, host-key, transport, or unexpected Git errors fail the SSH source instead of silently producing an empty backup.
4. Keep a local bare mirror in a source-specific work directory. The first run uses `git clone --mirror`; subsequent runs set the credential-free remote URL and use `git remote update --prune`.
5. Git commands receive the private key through `GIT_SSH_COMMAND` with `IdentitiesOnly=yes` and `BatchMode=yes`. The key path is never written into the repository's remote URL or Git config. GitHub continues to use its ephemeral credential helper and also keeps its token out of the URL.

The SSH Git URL uses the repository path supplied by the user, so a server only needs to provide normal Git-over-SSH access to those repositories. It does not need to provide a general-purpose shell for the discovery step.

## Incremental backup behavior

The discovered `git ls-remote` ref listing is normalized and used as the SSH source state. A small `<repo>.state` object is stored alongside the archive under the SSH prefix after a successful archive upload. If the state and an archive object already exist and the remote ref state is unchanged, the repository is skipped. The state written after synchronization is derived from the local mirror, so it describes the contents actually archived.

GitHub keeps its existing timestamp-based filtering. Existing archive suffixes `.tar.zst`, `.7z`, and `.7z.gpg` remain recognized when determining whether an archive exists. The new state sidecar is an internal metadata object and is not treated as an archive.

GitHub and SSH repositories use distinct local `clone/<source>/` and `archive/<source>/` directories in addition to their distinct S3 prefixes, preventing same-named repositories from colliding during one run.

## Archive and encryption behavior

Both sources use the existing mirror archive and layered encryption pipeline:

- empty or unset `BACKUP_PASSWORD` leaves the 7z layer unencrypted;
- a non-empty `BACKUP_PASSWORD` enables 7z password encryption;
- `GPG_PUBLIC_KEY_DIR` applies the existing GPG layer and supports multiple public key files;
- when both layers are configured, restore requires the GPG private key and the 7z password.

SSH repositories use the configured SSH S3 prefix for archive and state objects. GitHub repositories continue to use the existing prefix and object names.

## Failure behavior

An SFTP discovery failure, SSH authentication/host-key failure, unexpected Git probe failure, mirror synchronization failure, archive failure, archive upload failure, or state upload failure records the affected source/repository as failed. The process continues with other repositories and exits non-zero after the run if any failed, preserving the container-runtime failure reporting fix.

Invalid non-Git child directories are skipped with a debug message. They are not failures because the configured root is intentionally a directory containing a mixture of repositories and other directories.

## Non-goals

- Downloading working-tree files through SFTP; the backup is a Git mirror synchronized through Git's protocol.
- Running arbitrary commands on the SSH server.
- Changing GitHub discovery, archive formats, restore instructions, or existing GPG behavior beyond making the pipeline source-neutral.
