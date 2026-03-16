# GitHub Backup

A rust script to backup all repositories of a GitHub user.

Enviroment variables:
- `GITHUB_USERNAME`: The GitHub username to backup.
- `GITHUB_TOKEN`: The GitHub personal access token with `repo` scope to access the repositories.
- `WORK_DIR`: The directory for temporary files and logs. Default is `./backup`.
- `PER_PAGE`: The number of repositories to fetch from GitHub api per page. Default is `100`.
