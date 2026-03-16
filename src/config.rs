pub static CONFIG: once_cell::sync::Lazy<Config> = once_cell::sync::Lazy::new(|| {
    Config::from_env().expect("Failed to load configuration from environment variables")
});

pub struct Config {
    pub github_username: String,
    pub github_token: String,
    pub per_page: usize,
}
impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let github_username = std::env::var("GITHUB_USERNAME")?;
        let github_token = std::env::var("GITHUB_TOKEN")?;
        let per_page = std::env::var("PER_PAGE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(100);
        Ok(Self {
            github_username,
            github_token,
            per_page,
        })
    }
}
