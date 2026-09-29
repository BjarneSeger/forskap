//! `org.kde.krunner1`: KRunner sends the whole query to `Match`, shows the
//! returned matches under their category and calls `Run` for the pick,
//! preceded by `SetActivationToken` on Wayland. Registered through the
//! `krunner/dbusplugins` desktop file `forskap integration search-provider install` writes;
//! `X-Plasma-API=DBus2` there makes KRunner honour [`Runner::config`].

use std::collections::HashMap;

use serde::Serialize;
use zbus::zvariant::{OwnedValue, Type};

use super::{Provider, failed, ov};

/// Wire shape `(sssida{sv})`; field order is the contract, names are not.
#[derive(Serialize, Type)]
pub(super) struct Match {
    id: String,
    text: String,
    icon: String,
    /// `QueryMatch::CategoryRelevance`: 100 = Highest, 70 = High.
    category_relevance: i32,
    relevance: f64,
    properties: HashMap<String, OwnedValue>,
}

pub(super) struct Runner(pub Provider);

#[zbus::interface(name = "org.kde.krunner1")]
impl Runner {
    fn actions(&self) -> Vec<(String, String, String)> {
        vec![]
    }

    /// Read once by KRunner at startup. With a trigger word set, KRunner
    /// stops calling us for any other input.
    fn config(&self) -> HashMap<String, OwnedValue> {
        let mut cfg = HashMap::new();
        cfg.insert("MinLetterCount".to_string(), ov(2i32));
        if let Some(word) = self.0.trigger_word() {
            cfg.insert("TriggerWords".to_string(), ov(vec![word.to_string()]));
        }
        cfg
    }

    #[zbus(name = "Match")]
    async fn matches(&self, query: String) -> zbus::fdo::Result<Vec<Match>> {
        self.0.touch();
        let hits = self.0.search(&query).await.map_err(failed)?;
        let category_relevance = if hits.exact { 100 } else { 70 };
        Ok(hits
            .rows
            .into_iter()
            .map(|row| {
                let mut properties = HashMap::new();
                properties.insert("subtext".to_string(), ov(row.subtitle.as_str()));
                properties.insert("category".to_string(), ov("GitLab"));
                properties.insert("urls".to_string(), ov(vec![row.url.clone()]));
                Match {
                    id: row.id,
                    text: row.title,
                    icon: row.kind.icon().to_string(),
                    category_relevance,
                    // Frequently opened items float up; the cap keeps a
                    // favourite from pinning every other row to the floor.
                    relevance: 0.5 + f64::from(row.score.clamp(0, 50) as i32) / 100.0,
                    properties,
                }
            })
            .collect())
    }

    async fn run(&self, match_id: String, action_id: String) -> zbus::fdo::Result<()> {
        let _ = action_id;
        self.0.touch();
        self.0.activate(&match_id).await.map_err(failed)
    }

    fn set_activation_token(&self, token: String) {
        self.0.set_activation_token(token);
    }

    fn teardown(&self) {
        // The query session ended; rows stay cached since `Run` may follow.
        self.0.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_list_has_the_krunner_signature() {
        assert_eq!(<Vec<Match> as Type>::SIGNATURE.to_string(), "a(sssida{sv})");
        assert_eq!(
            <Vec<(String, String, String)> as Type>::SIGNATURE.to_string(),
            "a(sss)"
        );
    }
}
