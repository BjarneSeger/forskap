//! The background sync layer: fetches GitLab resources into one generic store
//! on a jittered schedule. Request handlers only ever read that store.

pub mod model;
pub mod schedule;
pub mod store;
