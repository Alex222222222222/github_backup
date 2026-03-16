extern crate dotenv;
use dotenv::dotenv;

use github_backup::repo;
use log::debug;

#[tokio::main]
async fn main() {
    env_logger::init();

    debug!("Starting GitHub backup process...");

    dotenv().ok();

    let repos = repo::get_all_repos()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| {
            // never archived, or archived before the last update
            r.archive_date.map(|d| d < r.updated_at).unwrap_or(true)
        });
    for repo in repos {
        println!("Cloning repo: {}", repo.name);
        if let Err(e) = repo::clone_repo(&repo).await {
            eprintln!("Failed to clone repo {}: {}", repo.name, e);
            continue;
        }
        println!("Archiving repo: {}", repo.name);
        if let Err(e) = repo::archive_repo(&repo).await {
            eprintln!("Failed to archive repo {}: {}", repo.name, e);
            continue;
        }
    }
}
