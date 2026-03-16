pub static CONFIG: once_cell::sync::Lazy<Config> = once_cell::sync::Lazy::new(|| {
    Config::from_env().expect("Failed to load configuration from environment variables")
});

pub struct Config {
    pub github_username: String,
    pub github_token: String,
    pub per_page: usize,
    pub work_dir: String,

    pub s3_endpoint: String,
    pub s3_access_key_id: String,
    pub s3_access_key: String,
    pub s3_bucket_name: String,
    pub s3_path_prefix: String,
    pub s3_virtual_hosted_style_request: bool,
    pub s3_region: Option<String>,
}
impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            github_username: std::env::var("GITHUB_USERNAME")?,
            github_token: std::env::var("GITHUB_TOKEN")?,
            per_page: std::env::var("PER_PAGE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100),
            work_dir: std::env::var("WORK_DIR").unwrap_or("./backup".to_string()),
            s3_endpoint: std::env::var("S3_ENDPOINT")?,
            s3_access_key_id: std::env::var("S3_ACCESS_KEY_ID")?,
            s3_access_key: std::env::var("S3_ACCESS_KEY")?,
            s3_bucket_name: std::env::var("S3_BUCKET_NAME")?,
            s3_path_prefix: std::env::var("S3_PATH_PREFIX")?,
            s3_virtual_hosted_style_request: std::env::var("S3_VIRTUAL_HOSTED_STYLE_REQUEST")
                .ok()
                .and_then(|s| s.parse::<bool>().ok())
                .unwrap_or(false),
            s3_region: std::env::var("S3_REGION").ok(),
        })
    }
}
