# GitHub Backup

A rust script to backup all repositories of a GitHub user.

Enviroment variables:
- `GITHUB_USERNAME`: The GitHub username to backup.
- `GITHUB_TOKEN`: The GitHub personal access token with `repo` scope to access the repositories.
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
