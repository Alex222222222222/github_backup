use object_store::aws::AmazonS3Builder;

use crate::config::CONFIG;

pub static S3_OBJECT_STORE: once_cell::sync::Lazy<
    tokio::sync::Mutex<std::sync::Arc<dyn object_store::ObjectStore>>,
> = once_cell::sync::Lazy::new(|| {
    create_remote_s3_object_store().expect("Failed to create remote S3 object store")
});

fn create_remote_s3_object_store()
-> anyhow::Result<tokio::sync::Mutex<std::sync::Arc<dyn object_store::ObjectStore>>> {
    let mut store = AmazonS3Builder::new()
        .with_endpoint(&CONFIG.s3_endpoint)
        .with_access_key_id(&CONFIG.s3_access_key_id)
        .with_secret_access_key(&CONFIG.s3_access_key)
        .with_bucket_name(&CONFIG.s3_bucket_name)
        .with_virtual_hosted_style_request(CONFIG.s3_virtual_hosted_style_request);
    if let Some(region) = CONFIG.s3_region.as_ref() {
        store = store.with_region(region);
    }

    Ok(tokio::sync::Mutex::new(std::sync::Arc::new(store.build()?)))
}
