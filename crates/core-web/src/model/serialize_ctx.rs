#[cfg(feature = "wasm")]
use {gloo_utils::format::JsValueSerdeExt, wasm_bindgen::JsValue};

pub use model::*;

#[cfg(feature = "wasm")]
pub fn serialize_ctx(ctx: &rillio_core::models::ctx::Ctx) -> JsValue {
    <JsValue as JsValueSerdeExt>::from_serde(&model::Ctx::from(ctx)).expect("JsValue from Ctx")
}

mod model {
    use std::collections::HashMap;

    use chrono::{DateTime, Utc};
    use itertools::Itertools;
    use serde::Serialize;

    use rillio_core::deep_links::SearchHistoryItemDeepLinks;
    use rillio_core::types::{notifications::NotificationItem, resource::MetaItemId};
    use url::Url;

    use crate::{env::WebEnv, model::deep_links_ext::DeepLinksExt};

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Ctx<'a> {
        pub profile: Profile<'a>,
        pub notifications: Notifications<'a>,
        pub search_history: Vec<SearchHistoryItem<'a>>,
        pub streaming_server_urls: Vec<StreamingServerUrlItem>,
        /// Compact, id-keyed view of the whole library so the web app can look up
        /// a meta item's membership/watched state at render without loading a
        /// per-item model. `removed == false` means the item is in the library.
        pub library: HashMap<MetaItemId, LibraryEntry>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LibraryEntry {
        pub removed: bool,
        pub watched: bool,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct StreamingServerUrlItem {
        pub url: Url,
        pub mtime: DateTime<Utc>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Notifications<'a> {
        /// Override the notifications to simplify the mapping
        pub items: HashMap<MetaItemId, Vec<&'a NotificationItem>>,
        pub last_updated: Option<DateTime<Utc>>,
        pub created: DateTime<Utc>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SearchHistoryItem<'a> {
        pub query: &'a String,
        pub deep_links: SearchHistoryItemDeepLinks,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Profile<'a> {
        #[serde(flatten)]
        profile: &'a rillio_core::types::profile::Profile,
        #[serde(skip_serializing_if = "Option::is_none")]
        auth: Option<Auth<'a>>,
    }

    #[derive(Serialize)]
    pub struct Auth<'a> {
        pub key: rillio_core::types::profile::AuthKey,
        pub user: User<'a>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct User<'a> {
        #[serde(flatten)]
        pub user: &'a rillio_core::types::profile::User,
        pub is_new_user: bool,
    }

    impl<'a> From<&'a rillio_core::models::ctx::Ctx> for Ctx<'a> {
        fn from(ctx: &'a rillio_core::models::ctx::Ctx) -> Self {
            Self {
                profile: Profile {
                    profile: &ctx.profile,
                    auth: ctx.profile.auth.as_ref().map(|auth| Auth {
                        key: auth.key.clone(),
                        user: User {
                            user: &auth.user,
                            is_new_user: auth.user.is_new_user::<WebEnv>(),
                        },
                    }),
                },
                notifications: Notifications {
                    items: ctx
                        .notifications
                        .items
                        .iter()
                        .map(|(meta_id, notifications)| {
                            (meta_id.to_owned(), notifications.values().collect())
                        })
                        .collect(),
                    last_updated: ctx.notifications.last_updated,
                    created: ctx.notifications.created,
                },
                search_history: ctx
                    .search_history
                    .items
                    .iter()
                    .sorted_by(|(_, a_date), (_, b_date)| Ord::cmp(b_date, a_date))
                    .map(|(query, ..)| SearchHistoryItem {
                        query,
                        deep_links: SearchHistoryItemDeepLinks::from(query).into_web_deep_links(),
                    })
                    .collect(),
                streaming_server_urls: ctx
                    .streaming_server_urls
                    .items
                    .iter()
                    .map(|(url, mtime)| StreamingServerUrlItem {
                        url: url.clone(),
                        mtime: *mtime,
                    })
                    .sorted_by(|a, b| Ord::cmp(&a.mtime, &b.mtime))
                    .collect(),
                library: ctx
                    .library
                    .items
                    .iter()
                    .map(|(id, library_item)| {
                        (
                            id.to_owned(),
                            LibraryEntry {
                                removed: library_item.removed,
                                watched: library_item.watched(),
                            },
                        )
                    })
                    .collect(),
            }
        }
    }
}
