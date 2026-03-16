extern crate dotenv;
use std::path::PathBuf;

use dotenv::dotenv;

use github_backup::repo;
use log::{debug, error, info};

#[tokio::main]
async fn main() {
    env_logger::init();

    debug!("Starting GitHub backup process...");

    dotenv().ok();

    // let repos = repo::get_all_repos()
    //     .await
    //     .unwrap()
    //     .into_iter()
    //     .filter(|r| {
    //         // never archived, or archived before the last update
    //         r.archive_date.map(|d| d < r.updated_at).unwrap_or(true)
    //     });
    // for repo in repos {
    //     info!("Cloning repo: {}", repo.name);
    //     if let Err(e) = repo::clone_repo(&repo).await {
    //         error!("Failed to clone repo {}: {}", repo.name, e);
    //         continue;
    //     }
    //     info!("Archiving repo: {}", repo.name);
    //     if let Err(e) = repo::archive_repo(&repo).await {
    //         error!("Failed to archive repo {}: {}", repo.name, e);
    //         continue;
    //     }
    //     info!("Uploading archive of repo: {}", repo.name);
    //     if let Err(e) = repo::upload_archive(&repo).await {
    //         error!("Failed to upload archive of repo {}: {}", repo.name, e);
    //         continue;
    //     }
    // }

    // let origin = PathBuf::from("/Users/zifanhua/Code/github_backup/README.md");
    let origin = PathBuf::from("/Users/zifanhua/Code/github_backup/test.7z");
    let target = object_store::path::Path::parse("test/test1.7z").unwrap();
    repo::upload_muiltipart(&origin, &target).await.unwrap();
}
