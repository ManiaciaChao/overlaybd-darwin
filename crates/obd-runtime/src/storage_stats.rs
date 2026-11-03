//! On-demand metadata sampling on the existing control thread, outside guest I/O.
use crate::Engine;
use anyhow::{ensure, Result};
use obd_core_adapter::BlockImage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpperPayload {
    pub unique_plaintext_bytes: u64,
    pub encoded_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageMetrics {
    /// Nonzero mapped bytes in the merged view; not guest filesystem used space.
    pub logical_data_bytes: u64,
    /// Encoded unique upper objects. Excludes foreground plaintext buffers, zeros, metadata and lowers.
    pub upper_payload: Option<UpperPayload>,
}
impl StorageMetrics {
    pub fn validate(&self, capacity: u64) -> Result<()> {
        ensure!(
            self.logical_data_bytes <= capacity,
            "logical sample exceeds capacity"
        );
        if let Some(p) = &self.upper_payload {
            ensure!(
                p.unique_plaintext_bytes <= capacity.next_multiple_of(4096)
                    && p.unique_plaintext_bytes.is_multiple_of(4096)
                    && p.encoded_bytes <= p.unique_plaintext_bytes
                    && (p.encoded_bytes == 0) == (p.unique_plaintext_bytes == 0),
                "invalid upper payload sample"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageReport {
    pub request: String,
    pub metrics: Option<StorageMetrics>,
    pub error: Option<String>,
}
pub async fn measure(image: &BlockImage) -> Result<StorageMetrics> {
    Ok(StorageMetrics {
        logical_data_bytes: image.sampled_logical_data_bytes().await?,
        upper_payload: image.packed_stats().await?.map(|s| UpperPayload {
            unique_plaintext_bytes: s.unique_plaintext_bytes,
            encoded_bytes: s.live_encoded_bytes,
        }),
    })
}
impl Engine {
    pub(crate) fn poll_storage_stats(&self, response: &Value) {
        let mut report = self.storage_report.lock().unwrap();
        if report
            .as_ref()
            .is_some_and(|r| response["storage_ack"].as_str() == Some(&r.request))
        {
            *report = None;
        }
        let Some(request) = response["sample_storage"].as_str() else {
            return;
        };
        if report.as_ref().is_some_and(|r| r.request == request) {
            return;
        }
        // ActiveIo pins this image version until sampling ends; no engine mutex
        // is held during tree traversal and no flush/barrier is requested.
        let result = self
            .begin_io()
            .and_then(|io| crate::runtime().block_on(measure(&io.image)));
        let (metrics, error) = match result {
            Ok(metrics) => (Some(metrics), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        *report = Some(StorageReport {
            request: request.into(),
            metrics,
            error,
        });
    }
}
