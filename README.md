# GitHub Backup

A rust script to backup all repositories of a GitHub user.

Enviroment variables:
- `GITHUB_USERNAME`: The GitHub username to backup.
- `GITHUB_TOKEN`: The GitHub personal access token with `repo` scope to access the repositories.
- `BACKUP_PASSWORD`: Optional password for the 7z layer of new backups. If empty or unset, the 7z layer is not password-protected; a separate GPG layer can still be enabled with `GPG_PUBLIC_KEY_DIR`.
- `GPG_PUBLIC_KEY_DIR`: Optional directory containing one exported GPG public key per regular file. If set, new archives are written as `.7z.gpg` and every listed public key can decrypt them. If both this and `BACKUP_PASSWORD` are configured, both credentials are required to restore a backup.
- `WORK_DIR`: The directory for temporary files and logs. Default is `./backup`.
- `PER_PAGE`: The number of repositories to fetch from GitHub api per page. Default is `100`.
- `RUST_LOG`: The log level for the script. Default is `info`.

S3 configuration through environment variables:
- `S3_ENDPOINT`: The endpoint URL for the S3-compatible storage service. Required.
- `S3_ACCESS_KEY_ID`: The access key ID for the S3-compatible storage service. Required.
- `S3_ACCESS_KEY`: The secret access key for the S3-compatible storage service. Required.
- `S3_BUCKET_NAME`: The name of the S3 bucket to upload the backup files to. Required.
- `S3_PATH_PREFIX`: The prefix for the backup files in the S3 bucket. Required. Should end with a slash (`/`). For example, `/github-backup/`.
- `S3_VIRTUAL_HOSTED_STYLE_REQUEST`: Whether to use virtual hosted-style requests for S3. Default is `false`. If set to `true`, the bucket name will be included in the endpoint URL (e.g., `https://my-bucket.s3.amazonaws.com`). If set to `false`, the bucket name will be included in the request path (e.g., `https://s3.amazonaws.com/my-bucket`). Default is `false`.
- `S3_REGION`: The region for the S3-compatible storage service. Optional. If not set, the script will attempt to guess the region automatically based on the endpoint URL and the bucket name.

## Usage:

Through docker:
```bash
docker run --rm \
  -e GITHUB_USERNAME=your_github_username \
  -e GITHUB_TOKEN=your_github_token \
  -e BACKUP_PASSWORD=your_backup_password \
  -e GPG_PUBLIC_KEY_DIR=/gpg-public-keys \
  -e S3_ENDPOINT=your_s3_endpoint \
  -e S3_ACCESS_KEY_ID=your_s3_access_key_id \
  -e S3_ACCESS_KEY=your_s3_access_key \
  -e S3_BUCKET_NAME=your_s3_bucket_name \
  -e S3_PATH_PREFIX=your_s3_path_prefix \
  -e S3_VIRTUAL_HOSTED_STYLE_REQUEST=true \
  -v /path/to/local/backup:/backup \
  -v /path/to/gpg-public-keys:/gpg-public-keys:ro \
  ghcr.io/alex222222222222/github-backup:latest
```

## Build Docker image locally:
```bash
docker build -t github-backup:local .
```

New backups are stored as `.7z` files when `GPG_PUBLIC_KEY_DIR` is unset. If `BACKUP_PASSWORD` is non-empty, they are password-protected. When `GPG_PUBLIC_KEY_DIR` is set, the `.7z` file is additionally encrypted with every public key in that directory and uploaded as `.7z.gpg`. This makes restoration require the corresponding GPG private key and, when configured, the 7z password.

```bash
gpg --output repository.7z --decrypt repository.7z.gpg
7zz x repository.7z
```

When `BACKUP_PASSWORD` was configured, `7zz` prompts for it during extraction. With multiple configured public keys, any one of the corresponding private keys can decrypt the GPG layer.

Existing `.tar.zst`, `.7z`, and `.7z.gpg` objects remain recognized. They are reused until the corresponding GitHub repository changes, at which point a new archive using the current configuration is uploaded.

The job continues processing other repositories after an individual clone, archive, or upload failure, but exits with a non-zero status after the run if any repository failed so container schedulers can mark the job as failed.
