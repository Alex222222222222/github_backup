use opendal::Operator;

use crate::config::CONFIG;

pub async fn create_remote_s3_object_store() -> anyhow::Result<Operator> {
    let mut store = opendal::services::S3::default()
        .endpoint(&CONFIG.s3_endpoint)
        .access_key_id(&CONFIG.s3_access_key_id)
        .secret_access_key(&CONFIG.s3_access_key)
        .bucket(&CONFIG.s3_bucket_name);
    if let Some(region) = CONFIG.s3_region.as_ref() {
        store = store.region(region);
    } else {
        let region =
            opendal::services::S3::detect_region(&CONFIG.s3_endpoint, &CONFIG.s3_bucket_name).await;
        if let Some(region) = region {
            store = store.region(&region);
        } else {
            anyhow::bail!(
                "Failed to detect S3 region for endpoint {} and bucket {}",
                CONFIG.s3_endpoint,
                CONFIG.s3_bucket_name
            );
        }
    }
    if CONFIG.s3_virtual_hosted_style_request {
        store = store.enable_virtual_host_style();
    }

    Ok(Operator::new(store)?.finish())
}
