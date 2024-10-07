use crate::{AgentMetadata, CheckpointSyncer};
use async_trait::async_trait;
use derive_new::new;
use eyre::{bail, Result};
use hyperlane_core::{ReorgEvent, SignedAnnouncement, SignedCheckpointWithMessageId};
use std::fmt;
use ya_gcp::{
    storage::{
        api::{error::HttpStatusError, http::StatusCode, Error},
        ObjectError, StorageClient,
    },
    AuthFlow, ClientBuilder, ClientBuilderConfig,
};
use tracing::{info, error};

const LATEST_INDEX_KEY: &str = "gcsLatestIndexKey";
const METADATA_KEY: &str = "gcsMetadataKey";
const ANNOUNCEMENT_KEY: &str = "gcsAnnouncementKey";
const REORG_FLAG_KEY: &str = "gcsReorgFlagKey";
/// Path to GCS users_secret file
pub const GCS_USER_SECRET: &str = "GCS_USER_SECRET";
/// Path to GCS Service account key
pub const GCS_SERVICE_ACCOUNT_KEY: &str = "GCS_SERVICE_ACCOUNT_KEY";

/// Google Cloud Storage client builder
/// Provide `AuthFlow::NoAuth` for no-auth access to public bucket
/// # Example 1 - anonymous client with access to public bucket
/// ```
///    use hyperlane_base::GcsStorageClientBuilder;
///    use ya_gcp::AuthFlow;
/// #  #[tokio::main]
/// #  async fn main() {
///    let client = GcsStorageClientBuilder::new(AuthFlow::NoAuth)
///        .build("HyperlaneBucket", None)
///        .await.expect("failed to instantiate anonymous client");
/// #  }
///```
///
/// For authenticated write access to bucket proper file path must be provided.
/// # WARN: panic-s if file path is incorrect or data in it as faulty
///
/// # Example 2 - service account key
/// ```should_panic
///    use hyperlane_base::GcsStorageClientBuilder;
///    use ya_gcp::{AuthFlow, ServiceAccountAuth};
/// #  #[tokio::main]
/// #  async fn main() {
///    let auth =
///        AuthFlow::ServiceAccount(ServiceAccountAuth::Path("path/to/sac.json".into()));
///
///    let client = GcsStorageClientBuilder::new(auth)
///        .build("HyperlaneBucket", None)
///        .await.expect("failed to instantiate anonymous client");
/// #  }
///```
/// # Example 3 - user secret access
/// ```should_panic
///    use hyperlane_base::GcsStorageClientBuilder;
///    use ya_gcp::AuthFlow;
/// #  #[tokio::main]
/// #  async fn main() {
///    let auth =
///        AuthFlow::UserAccount("path/to/user_secret.json".into());
///
///    let client = GcsStorageClientBuilder::new(auth)
///        .build("HyperlaneBucket", None)
///        .await.expect("failed to instantiate anonymous client");
/// #  }
///```
#[derive(Debug, new)]
pub struct GcsStorageClientBuilder {
    auth: AuthFlow,
}

/// Google Cloud Storage client
/// Enables use of any of service account key OR user secrets to authenticate
/// For anonymous access to public data provide `(None, None)` to Builder
pub struct GcsStorageClient {
    // GCS storage client
    // # Details: <https://docs.rs/ya-gcp/latest/ya_gcp/storage/struct.StorageClient.html>
    inner: StorageClient,
    // bucket name of this client's storage
    bucket: String,
    // folder name of this client's storage
    folder: Option<String>,
}

impl GcsStorageClientBuilder {
    /// Instantiates `ya_gcp:StorageClient` based on provided auth method
    /// # Param
    /// * `baucket_name` - String name of target bucket to work with, will be used by all store and get ops
    pub async fn build(
        self,
        bucket_name: impl Into<String>,
        folder: Option<String>,
    ) -> Result<GcsStorageClient> {
        let inner = ClientBuilder::new(ClientBuilderConfig::new().auth_flow(self.auth))
            .await?
            .build_storage_client();

        let bucket = bucket_name.into();
        let folder = folder;

        GcsStorageClient::validate_bucket_name(&bucket)?;
        Ok(GcsStorageClient { inner, bucket, folder })
    }
}

impl GcsStorageClient {
    // convenience formatter
    fn get_checkpoint_key(index: u32) -> String {
        format!("checkpoint_{index}_with_id.json")
    }

    fn object_path(&self, object_name: &str) -> String {
        if let Some(folder) = &self.folder {
            format!("{}/{}", folder.trim_end_matches('/'), object_name)
        } else {
            object_name.to_string()
        }
    }

    fn validate_bucket_name(bucket: &str) -> Result<()> {
        if bucket.contains('/') {
            error!("Bucket name '{}' has an invalid symbol '/'", bucket);
            bail!("Bucket name '{}' has an invalid symbol '/'", bucket);
        } else {
            Ok(())
        }
    }

    // #test only method[s]
    #[cfg(test)]
    pub(crate) async fn get_by_path(&self, path: impl AsRef<str>) -> Result<()> {
        self.inner.get_object(&self.bucket, path).await?;
        Ok(())
    }
}

// Required by `CheckpointSyncer`
impl fmt::Debug for GcsStorageClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcsStorageClient")
            .field("bucket", &self.bucket)
            .field("folder", &self.folder)
            .finish()
    }
}

#[async_trait]
impl CheckpointSyncer for GcsStorageClient {
    /// Read the highest index of this Syncer
    async fn latest_index(&self) -> Result<Option<u32>> {
        match self.inner.get_object(&self.bucket, LATEST_INDEX_KEY).await {
            Ok(data) => Ok(Some(serde_json::from_slice(data.as_ref())?)),
            Err(e) => match e {
                // never written before to this bucket
                ObjectError::InvalidName(_) => Ok(None),
                ObjectError::Failure(Error::HttpStatus(HttpStatusError(StatusCode::NOT_FOUND))) => {
                    Ok(None)
                }
                _ => bail!(e),
            },
        }
    }

    /// Writes the highest index of this Syncer
    async fn write_latest_index(&self, index: u32) -> Result<()> {
        let d = serde_json::to_vec(&index)?;
        self.inner
            .insert_object(&self.bucket, LATEST_INDEX_KEY, d)
            .await?;
        Ok(())
    }

    /// Update the latest index of this syncer if necessary
    async fn update_latest_index(&self, index: u32) -> Result<()> {
        let curr = self.latest_index().await?.unwrap_or(0);
        if index > curr {
            self.write_latest_index(index).await?;
        }
        Ok(())
    }

    /// Attempt to fetch the signed (checkpoint, messageId) tuple at this index
    async fn fetch_checkpoint(&self, index: u32) -> Result<Option<SignedCheckpointWithMessageId>> {
        match self
            .inner
            .get_object(&self.bucket, GcsStorageClient::get_checkpoint_key(index))
            .await
        {
            Ok(data) => Ok(Some(serde_json::from_slice(data.as_ref())?)),
            Err(e) => match e {
                ObjectError::Failure(Error::HttpStatus(HttpStatusError(StatusCode::NOT_FOUND))) => {
                    Ok(None)
                }
                _ => bail!(e),
            },
        }
    }

    /// Write the signed (checkpoint, messageId) tuple to this syncer
    async fn write_checkpoint(
        &self,
        signed_checkpoint: &SignedCheckpointWithMessageId,
    ) -> Result<()> {
        self.inner
            .insert_object(
                &self.bucket,
                GcsStorageClient::get_checkpoint_key(signed_checkpoint.value.index),
                serde_json::to_vec(signed_checkpoint)?,
            )
            .await?;
        Ok(())
    }

    /// Write the agent metadata to this syncer
    async fn write_metadata(&self, metadata: &AgentMetadata) -> Result<()> {
        let object_name = self.object_path(METADATA_KEY);
        let serialized_metadata = serde_json::to_string_pretty(metadata)?;

        match self.inner.insert_object(&self.bucket, &object_name, serialized_metadata.into_bytes()).await {
            Ok(_) => {
                info!("Successfully uploaded metadata to '{}'", object_name);
                Ok(())
            }
            Err(e) => {
                error!("Failed to upload metadata to '{}': {:?}", object_name, e);
                Err(e.into())
            }
        }
    }

    /// Write the signed announcement to this syncer
    async fn write_announcement(&self, announcement: &SignedAnnouncement) -> Result<()> {
        let object_name = self.object_path("announcement.json");
        let data = serde_json::to_vec(announcement)?;

        match self.inner.insert_object(&self.bucket, &object_name, data).await {
            Ok(_) => {
                info!("Successfully uploaded announcement to '{}'", object_name);
                Ok(())
            }
            Err(e) => {
                error!("Failed to upload announcement to '{}': {:?}", object_name, e);
                Err(e.into())
            }
        }
    }

    /// Return the announcement storage location for this syncer
    fn announcement_location(&self) -> String {
        let location = format!("gs://{}/{}", &self.bucket, self.object_path(ANNOUNCEMENT_KEY));
        location
    }

    async fn write_reorg_status(&self, reorged_event: &ReorgEvent) -> Result<()> {
        let serialized_metadata = serde_json::to_string_pretty(reorged_event)?;
        self.inner
            .insert_object(&self.bucket, REORG_FLAG_KEY, serialized_metadata)
            .await?;
        Ok(())
    }

    async fn reorg_status(&self) -> Result<Option<ReorgEvent>> {
        Ok(None)
    }
}

#[tokio::test]
async fn public_landset_no_auth_works_test() {
    const LANDSAT_BUCKET: &str = "gcp-public-data-landsat";
    const LANDSAT_KEY: &str = "LC08/01/001/003/LC08_L1GT_001003_20140812_20170420_01_T2/LC08_L1GT_001003_20140812_20170420_01_T2_B3.TIF";
    let client = GcsStorageClientBuilder::new(AuthFlow::NoAuth)
        .build(LANDSAT_BUCKET, None)
        .await
        .unwrap();
    assert!(client.get_by_path(LANDSAT_KEY).await.is_ok());
}
