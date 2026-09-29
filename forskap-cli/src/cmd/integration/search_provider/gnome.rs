//! `org.gnome.Shell.SearchProvider2`: GNOME Shell asks for result ids on
//! every keystroke, then for the metas of the ids it will show, and finally
//! to activate one. Registered through the `.ini` and `.desktop` files that
//! `forskap integration search-provider install` writes.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

use super::{Provider, failed, ov};

pub(super) struct SearchProvider2(pub Provider);

impl SearchProvider2 {
    async fn ids(&self, terms: &[String]) -> zbus::fdo::Result<Vec<String>> {
        self.0.touch();
        let hits = self.0.search(&terms.join(" ")).await.map_err(failed)?;
        Ok(hits.rows.into_iter().map(|r| r.id).collect())
    }
}

#[zbus::interface(name = "org.gnome.Shell.SearchProvider2")]
impl SearchProvider2 {
    async fn get_initial_result_set(&self, terms: Vec<String>) -> zbus::fdo::Result<Vec<String>> {
        self.ids(&terms).await
    }

    /// Every search is a cheap cache read, so the previous set is not
    /// narrowed but replaced.
    async fn get_subsearch_result_set(
        &self,
        previous_results: Vec<String>,
        terms: Vec<String>,
    ) -> zbus::fdo::Result<Vec<String>> {
        let _ = previous_results;
        self.ids(&terms).await
    }

    /// One meta per requested id, in order; an id we no longer know gets a
    /// placeholder rather than an error, which would blank the whole section.
    async fn get_result_metas(
        &self,
        identifiers: Vec<String>,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.0.touch();
        Ok(identifiers
            .iter()
            .map(|id| {
                let mut meta = HashMap::new();
                meta.insert("id".to_string(), ov(id.as_str()));
                match self.0.cached(id) {
                    Some(row) => {
                        meta.insert("name".to_string(), ov(row.title.as_str()));
                        meta.insert("description".to_string(), ov(row.subtitle.as_str()));
                        // A serialized GIcon: a path reads as a file icon,
                        // a bare name as a themed one.
                        meta.insert("gicon".to_string(), ov(row.icon()));
                        meta.insert("clipboardText".to_string(), ov(row.url.as_str()));
                    }
                    None => {
                        meta.insert("name".to_string(), ov(id.as_str()));
                    }
                }
                meta
            })
            .collect())
    }

    async fn activate_result(
        &self,
        identifier: String,
        terms: Vec<String>,
        timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        let _ = (terms, timestamp);
        self.0.touch();
        self.0.activate(&identifier).await.map_err(failed)
    }

    async fn launch_search(&self, terms: Vec<String>, timestamp: u32) -> zbus::fdo::Result<()> {
        let _ = timestamp;
        self.0.touch();
        self.0.launch_search(&terms.join(" ")).await.map_err(failed)
    }
}
