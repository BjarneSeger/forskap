//! The background sync layer: fetches GitLab resources into one generic store
//! on a jittered schedule. Request handlers only ever read that store.
//!
//! - [`model`]: typed mirrors of the GitLab resources.
//! - [`store`]: their fjall tables plus views, job states and identity.
//! - [`jobs`]: what each job fetches and how it lands.
//! - [`planner`]: which jobs to keep scheduled (the tracked projects).
//! - [`schedule`]: jittered due times and backoff, as pure functions.
//! - [`engine`]: the single worker running it all, and the handlers' handle.
//! - [`avatars`]: the project avatar files and the rows naming them.

pub mod avatars;
pub mod engine;
pub mod jobs;
pub mod model;
pub mod planner;
pub mod schedule;
pub mod store;

pub use avatars::AvatarDir;
pub use engine::{Clear, JobInfo, JobStatus, Snapshot, SyncHandle};
pub use jobs::Job;

/// Unix seconds; 0 for a clock before the epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
