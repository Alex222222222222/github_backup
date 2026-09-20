# GitHub Backup

This program mirrors Git repositories and uploads encrypted 7z archives to S3-compatible storage. It supports GitHub repositories, repositories discovered under a remote SSH directory, or both at the same time.

## Sources

### GitHub

Configure both variables to enable the GitHub source:

- `GITHUB_USERNAME`: GitHub username.
- `GITHUB_TOKEN`: GitHub personal access token with `repo` scope.
- `S3_PATH_PREFIX`: S3 prefix for GitHub archives, for example `github/`.

The token is supplied to Git through an ephemeral credential helper. It is not embedded in the remote URL or persisted in the mirrored repository's config.

### Remote SSH directory

Configure these required variables to enable the SSH source:

- `SSH_USERNAME`: SSH login username.
- `SSH_HOST`: SSH host name or address.
- `SSH_PORT`: optional SSH port; defaults to `22`.
- `SSH_DISABLE_HOST_KEY_CHECK`: optional boolean; defaults to `false`. Set to `true` only when host-key verification cannot be provisioned; this disables verification for both SFTP and Git-over-SSH.
- `SSH_PRIVATE_KEY_PATH`: path inside the container to a private key usable without an interactive passphrase prompt.
- `SSH_ROOT_DIR`: remote directory whose immediate child directories are inspected.
- `SSH_S3_PATH_PREFIX`: separate S3 prefix for SSH archives, for example `ssh/`.

The program uses the SSH connection's SFTP subsystem to list directories; it does not run arbitrary shell commands for discovery. Each child directory is then checked with `git ls-remote` and synchronized with `git clone --mirror` or `git remote update --prune` over Git's SSH transport. The SSH server therefore needs normal Git-over-SSH support for those repository paths and an SFTP subsystem, but does not need to provide a general-purpose interactive shell.

SSH connections use `SSH_PORT` for both SFTP and Git-over-SSH; the default is `22`. SSH host keys are verified against the app user's standard `/home/app/.ssh/known_hosts` file in the container. Mount a prepared `known_hosts` file at that path. For a non-default port, the entry normally uses the `[host]:port` form. Unknown or changed host keys cause the backup to fail rather than being accepted automatically. Git uses the same host-key policy through OpenSSH.

When `SSH_DISABLE_HOST_KEY_CHECK=true`, the application accepts any SSH host key and Git uses no persistent known-hosts file. This is insecure and makes man-in-the-middle attacks possible; leave it unset or `false` whenever a trusted `known_hosts` file can be mounted.

The SSH source lists only immediate directories under `SSH_ROOT_DIR`. Non-Git directories are skipped. Valid repositories are stored under `SSH_S3_PATH_PREFIX`; a small `<repository>.state` object records the synchronized Git refs so unchanged SSH repositories are not re-uploaded on later runs.

GitHub and SSH may be configured together. Keep their S3 prefixes separate, especially when repositories with the same name exist in both sources. Their temporary clone and archive directories are also separated locally.

At least one source must be configured. A partial GitHub or SSH configuration is rejected at startup.

## Encryption and common settings

- `BACKUP_PASSWORD`: optional password for the 7z layer. If empty or unset, the 7z archive is not password-protected.
- `GPG_PUBLIC_KEY_DIR`: optional directory containing one exported GPG public key per regular file. When set, archives receive an additional GPG layer and are uploaded as `.7z.gpg`. Multiple key files are supported; any matching private key can decrypt the GPG layer.
- `WORK_DIR`: temporary clone/archive directory. Default is `./backup`.
- `PER_PAGE`: GitHub API page size. Default is `100`.
- `RUST_LOG`: log level. Default is `info`.

S3 configuration:

- `S3_ENDPOINT`: S3-compatible endpoint. Required.
- `S3_ACCESS_KEY_ID`: access key ID. Required.
- `S3_ACCESS_KEY`: secret access key. Required.
- `S3_BUCKET_NAME`: bucket name. Required.
- `S3_VIRTUAL_HOSTED_STYLE_REQUEST`: use virtual-hosted-style requests. Default is `false`.
- `S3_REGION`: optional region; if omitted, the program attempts to detect it.

`S3_PATH_PREFIX` is required when GitHub is enabled. `SSH_S3_PATH_PREFIX` is required when SSH is enabled.

## Docker usage

Prepare the SSH host key in `/path/to/known_hosts` before running the container, for example by verifying it through a trusted channel. The private key and known-hosts file should be mounted read-only.

GitHub and SSH together:

```bash
docker run --rm \
  -e GITHUB_USERNAME=your_github_username \
  -e GITHUB_TOKEN=your_github_token \
  -e SSH_USERNAME=backup \
  -e SSH_HOST=git.example.test \
  -e SSH_PRIVATE_KEY_PATH=/run/secrets/ssh/id_ed25519 \
  -e SSH_ROOT_DIR=/srv/git \
  -e BACKUP_PASSWORD=your_backup_password \
  -e GPG_PUBLIC_KEY_DIR=/gpg-public-keys \
  -e WORK_DIR=/backup \
  -e S3_ENDPOINT=your_s3_endpoint \
  -e S3_ACCESS_KEY_ID=your_s3_access_key_id \
  -e S3_ACCESS_KEY=your_s3_access_key \
  -e S3_BUCKET_NAME=your_s3_bucket_name \
  -e S3_PATH_PREFIX=github/ \
  -e SSH_S3_PATH_PREFIX=ssh/ \
  -e S3_VIRTUAL_HOSTED_STYLE_REQUEST=true \
  -v /path/to/local/backup:/backup \
  -v /path/to/ssh-secrets:/run/secrets/ssh:ro \
  -v /path/to/known_hosts:/home/app/.ssh/known_hosts:ro \
  -v /path/to/gpg-public-keys:/gpg-public-keys:ro \
  ghcr.io/alex222222222222/github-backup:latest
```

SSH-only operation omits the GitHub variables and `S3_PATH_PREFIX`:

```bash
docker run --rm \
  -e SSH_USERNAME=backup \
  -e SSH_HOST=git.example.test \
  -e SSH_PRIVATE_KEY_PATH=/run/secrets/ssh/id_ed25519 \
  -e SSH_ROOT_DIR=/srv/git \
  -e SSH_S3_PATH_PREFIX=ssh/ \
  -e WORK_DIR=/backup \
  -e S3_ENDPOINT=your_s3_endpoint \
  -e S3_ACCESS_KEY_ID=your_s3_access_key_id \
  -e S3_ACCESS_KEY=your_s3_access_key \
  -e S3_BUCKET_NAME=your_s3_bucket_name \
  -v /path/to/local/backup:/backup \
  -v /path/to/ssh-secrets:/run/secrets/ssh:ro \
  -v /path/to/known_hosts:/home/app/.ssh/known_hosts:ro \
  ghcr.io/alex222222222222/github-backup:latest
```

## Restore

Without GPG:

```bash
7zz x repository.7z
```

With GPG:

```bash
gpg --output repository.7z --decrypt repository.7z.gpg
7zz x repository.7z
```

When `BACKUP_PASSWORD` was non-empty, `7zz` asks for that password during extraction. When `GPG_PUBLIC_KEY_DIR` was configured, the corresponding private key is required for the GPG step. If both layers were configured, both credentials are required.

Existing `.tar.zst`, `.7z`, and `.7z.gpg` objects remain recognized. They are reused until the corresponding source changes. The SSH `.state` sidecar is metadata used for change detection and is not part of the archive.

The job continues processing other repositories after an individual discovery, synchronization, archive, upload, or state-upload failure, then exits with a non-zero status if any repository failed so container schedulers can mark the job as failed.

## Build locally

```bash
docker build -t github-backup:local .
```
