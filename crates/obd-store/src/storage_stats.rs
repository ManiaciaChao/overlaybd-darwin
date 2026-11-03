//! Volatile, on-demand samples. Never part of the durable registry or I/O protocol.
use super::*;
use obd_runtime::storage_stats::StorageReport;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Pending {
    request: String,
    token: String,
    generation: u64,
    upper: String,
    started: Instant,
}
struct Completed {
    token: String,
    generation: u64,
    upper: String,
    value: Value,
}
#[derive(Default)]
pub(super) struct Samples {
    pending: BTreeMap<String, Pending>,
    completed: BTreeMap<String, Completed>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Store {
    pub async fn storage_stats(&mut self, id: &str, refresh: bool) -> Result<Value> {
        self.healthy(id)?;
        let image = self.image(id)?;
        let lease = self.state.leases.values().find(|l| l.image == id).cloned();
        if let Some(lease) = lease {
            if !lease.ready
                || !self.connected.contains(&lease.token)
                || lease.generation != image.generation
            {
                return Ok(
                    json!({"status":"unavailable", "error":"引擎尚未就绪或需要恢复，请稍后重试。"}),
                );
            }
            if !refresh {
                if let Some(done) = self.storage_samples.completed.get(id) {
                    if done.token == lease.token
                        && done.generation == image.generation
                        && done.upper == image.upper
                        && !self.storage_samples.pending.contains_key(id)
                    {
                        let mut value = done.value.clone();
                        value["usage"] = self.space_usage("image", id)?;
                        return Ok(value);
                    }
                }
            }
            let valid_pending = self.storage_samples.pending.get(id).is_some_and(|p| {
                p.token == lease.token && p.generation == image.generation && p.upper == image.upper
            });
            if refresh || !valid_pending {
                self.storage_samples.pending.insert(
                    id.into(),
                    Pending {
                        request: object_id(),
                        token: lease.token,
                        generation: image.generation,
                        upper: image.upper,
                        started: Instant::now(),
                    },
                );
            }
            if self.storage_samples.pending[id].started.elapsed().as_secs() >= 10 {
                self.storage_samples.pending.remove(id);
                return Ok(json!({"status":"unavailable", "error":"等待引擎统计超时，请重试。"}));
            }
            return Ok(json!({"status":"pending"}));
        }
        // Never open another upper while a managed device may acquire ownership.
        self.detached(id)?;
        let metrics =
            Self::worker(json!({"op":"storage-stats", "directory":self.directory(&image)})).await?;
        Ok(
            json!({"status":"ready", "source":"offline", "generation":image.generation,
            "upper":image.upper, "sampled_at":now(), "metrics":metrics,
            "usage":self.space_usage("image", id)?}),
        )
    }

    pub fn poll_with_storage_report(
        &mut self,
        a: &Attachment,
        receipt: Option<Receipt>,
        closed: bool,
        report: Option<StorageReport>,
    ) -> Result<Value> {
        let id = self
            .state
            .leases
            .get(&a.token)
            .context("attachment not found")?
            .image
            .clone();
        // Original credential, generation and lifecycle validation always runs
        // first. Statistics cannot authorize an attachment or alter durability.
        let mut value = self.poll(a, receipt, closed)?;
        if closed {
            self.storage_samples.pending.remove(&id);
            self.storage_samples.completed.remove(&id);
            return Ok(value);
        }
        let image = self.image(&id)?;
        if let Some(report) = report {
            value["storage_ack"] = json!(report.request);
            let expected = self.storage_samples.pending.get(&id).is_some_and(|p| {
                p.request == report.request
                    && p.token == a.token
                    && p.generation == a.generation
                    && p.generation == image.generation
                    && p.upper == image.upper
            });
            if expected {
                let checked = report
                    .metrics
                    .as_ref()
                    .context("引擎未返回统计数据。")
                    .and_then(|m| m.validate(image.geometry.virtual_size));
                let result = if report.error.is_none() && checked.is_ok() {
                    json!({"status":"ready", "source":"live", "generation":image.generation,
                        "upper":image.upper, "sampled_at":now(), "metrics":report.metrics})
                } else {
                    json!({"status":"unavailable", "error":"本次数据统计失败，请重试。"})
                };
                self.storage_samples.pending.remove(&id);
                self.storage_samples.completed.insert(
                    id.clone(),
                    Completed {
                        token: a.token.clone(),
                        generation: image.generation,
                        upper: image.upper.clone(),
                        value: result,
                    },
                );
            }
        }
        // A restack must finish and be acknowledged before pinning a read view.
        if self.state.leases[&a.token].ready
            && !self.state.pending_restacks.contains_key(&id)
            && value["command"].is_null()
        {
            if let Some(p) = self.storage_samples.pending.get(&id) {
                if p.token == a.token
                    && p.generation == a.generation
                    && p.upper == image.upper
                    && p.started.elapsed().as_secs() < 10
                {
                    value["sample_storage"] = json!(p.request);
                }
            }
        }
        Ok(value)
    }
}
