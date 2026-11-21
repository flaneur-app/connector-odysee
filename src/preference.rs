// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `shared` preference of the hosted wallet: follows, watch later, favourites, settings.
//!
//! It is the source of truth every Odysee client syncs, and a write replaces the whole object, so
//! a bug in a write would wipe the user's follows everywhere. This connector only reads it.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::Api;
use crate::links::claim_id_from_uri;
use flaneur_connector::{Error, Result};

/// Id of the built-in "watch later" collection.
pub(crate) const WATCH_LATER: &str = "watchlater";
/// Id of the built-in "favorites" collection.
pub(crate) const FAVORITES: &str = "favorites";

/// The `value` of the shared preference, as far as Flâneur reads it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SharedPreference {
    #[serde(default)]
    following: Vec<Follow>,
    /// Legacy list of channel URIs, kept in step with `following` by the web app.
    #[serde(default)]
    subscriptions: Vec<String>,
    #[serde(default, rename = "builtinCollections")]
    builtin_collections: std::collections::HashMap<String, BuiltinCollection>,
    #[serde(default)]
    settings: Value,
}

#[derive(Debug, Deserialize)]
struct Follow {
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BuiltinCollection {
    #[serde(default)]
    pub name: Option<String>,
    /// Permanent URIs of the items.
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Deserialize)]
struct Envelope {
    shared: Option<Shared>,
}

#[derive(Deserialize)]
struct Shared {
    #[serde(default)]
    value: Option<SharedPreference>,
}

impl SharedPreference {
    /// Reads the preference; `None` when the account never saved one.
    pub async fn fetch(api: &Api, token: &str) -> Result<Option<Self>> {
        let envelope: Value = api
            .proxy("preference_get", json!({"key": "shared"}), Some(token))
            .await?;
        // An account without preferences answers `{}` or `{"shared": null}`.
        if envelope.get("shared").is_none_or(Value::is_null) {
            return Ok(None);
        }
        let envelope: Envelope =
            serde_json::from_value(envelope).map_err(|e| Error::Parse(e.to_string()))?;
        Ok(envelope.shared.and_then(|s| s.value))
    }

    /// Claim ids of the followed channels, in the order the user followed them.
    pub fn followed_channel_ids(&self) -> Vec<String> {
        let uris = self
            .following
            .iter()
            .map(|f| f.uri.as_str())
            .chain(self.subscriptions.iter().map(String::as_str));
        let mut ids: Vec<String> = Vec::new();
        for id in uris.filter_map(claim_id_from_uri) {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_owned());
            }
        }
        ids
    }

    pub fn active_channel_claim(&self) -> Option<String> {
        self.settings
            .get("active_channel_claim")?
            .as_str()
            .map(str::to_owned)
    }

    pub fn builtin(&self, id: &str) -> Option<&BuiltinCollection> {
        self.builtin_collections.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_follows_and_collections() {
        let json = r#"{
            "following": [{"uri": "lbry://@Odysee#80d2590ad04e36fb1d077a9b9e3a8bba76defdf8", "notificationsDisabled": true}],
            "subscriptions": ["lbry://@Odysee#80d2590ad04e36fb1d077a9b9e3a8bba76defdf8", "lbry://@Other#1111111111111111111111111111111111111111", "lbry://@short#1"],
            "builtinCollections": {"watchlater": {"id": "watchlater", "name": "Watch Later", "items": ["lbry://x#ee58d95e7e42ec9479e13b28d57556f9ffb373dc"]}},
            "settings": {"active_channel_claim": "abc"},
            "unknown": {"kept": true}
        }"#;
        let p: SharedPreference = serde_json::from_str(json).unwrap();
        assert_eq!(
            p.followed_channel_ids(),
            [
                "80d2590ad04e36fb1d077a9b9e3a8bba76defdf8",
                "1111111111111111111111111111111111111111"
            ]
        );
        assert_eq!(p.active_channel_claim().as_deref(), Some("abc"));
        assert_eq!(p.builtin(WATCH_LATER).unwrap().items.len(), 1);
        assert!(p.builtin(FAVORITES).is_none());
    }
}
