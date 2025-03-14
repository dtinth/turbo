use std::{
    collections::btree_map::Entry,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Result};
use hyper::Uri;
use turbo_tasks::{TransientInstance, Value};
use turbopack_cli_utils::issue::ConsoleUiVc;

use super::{
    headers::{HeaderValue, Headers},
    query::Query,
    request::SourceRequest,
    ContentSourceContent, ContentSourceDataVary, ContentSourceResult, ContentSourceVc,
    ProxyResultVc, StaticContentVc,
};
use crate::{handle_issues, source::ContentSourceData};

/// The result of [`resolve_source_request`]. Similar to a
/// `ContentSourceContent`, but without the `Rewrite` variant as this is taken
/// care in the function.
#[turbo_tasks::value(serialization = "none")]
pub enum ResolveSourceRequestResult {
    NotFound,
    Static(StaticContentVc),
    HttpProxy(ProxyResultVc),
}

/// Resolves a [SourceRequest] within a [super::ContentSource], returning the
/// corresponding content.
#[turbo_tasks::function]
pub async fn resolve_source_request(
    source: ContentSourceVc,
    request: TransientInstance<SourceRequest>,
    console_ui: ConsoleUiVc,
) -> Result<ResolveSourceRequestResultVc> {
    let mut data = ContentSourceData::default();
    let mut current_source = source;
    // Remove leading slash.
    let original_path = request.uri.path().to_string();
    let mut current_asset_path = urlencoding::decode(&original_path[1..])?.into_owned();
    let mut request_overwrites = (*request).clone();
    loop {
        let result = current_source.get(&current_asset_path, Value::new(data));
        handle_issues(
            result,
            &original_path,
            "get content from source",
            console_ui,
        )
        .await?;

        match &*result.strongly_consistent().await? {
            ContentSourceResult::NotFound => break Ok(ResolveSourceRequestResult::NotFound.cell()),
            ContentSourceResult::NeedData(needed) => {
                current_source = needed.source.resolve().await?;
                current_asset_path = needed.path.clone();
                data = request_to_data(&request_overwrites, &needed.vary).await?;
            }
            ContentSourceResult::Result { get_content, .. } => {
                let content_vary = get_content.vary().await?;
                let content_data = request_to_data(&request_overwrites, &content_vary).await?;
                let content = get_content.get(Value::new(content_data));
                match &*content.await? {
                    ContentSourceContent::Rewrite(rewrite) => {
                        let rewrite = rewrite.await?;
                        // If a source isn't specified, we restart at the top.
                        let new_source = rewrite.source.unwrap_or(source);
                        let new_uri = Uri::try_from(&rewrite.path_and_query)?;
                        if new_source == current_source && new_uri == request_overwrites.uri {
                            bail!("rewrite loop detected: {}", new_uri);
                        }
                        let new_asset_path =
                            urlencoding::decode(&new_uri.path()[1..])?.into_owned();

                        current_source = new_source;
                        request_overwrites.uri = new_uri;
                        current_asset_path = new_asset_path;
                        data = ContentSourceData::default();
                    } // _ => ,
                    ContentSourceContent::NotFound => {
                        break Ok(ResolveSourceRequestResult::NotFound.cell())
                    }
                    ContentSourceContent::Static(static_content) => {
                        break Ok(ResolveSourceRequestResult::Static(*static_content).cell())
                    }
                    ContentSourceContent::HttpProxy(proxy_result) => {
                        break Ok(ResolveSourceRequestResult::HttpProxy(*proxy_result).cell())
                    }
                }
            }
        }
    }
}

static CACHE_BUSTER: AtomicU64 = AtomicU64::new(0);

async fn request_to_data(
    request: &SourceRequest,
    vary: &ContentSourceDataVary,
) -> Result<ContentSourceData> {
    let mut data = ContentSourceData::default();
    if vary.method {
        data.method = Some(request.method.clone());
    }
    if vary.url {
        data.url = Some(request.uri.to_string());
    }
    if vary.body {
        data.body = Some(request.body.clone().into());
    }
    if let Some(filter) = vary.query.as_ref() {
        if let Some(query) = request.uri.query() {
            let mut query: Query = serde_qs::from_str(query)?;
            query.filter_with(filter);
            data.query = Some(query);
        } else {
            data.query = Some(Query::default())
        }
    }
    if let Some(filter) = vary.headers.as_ref() {
        let mut headers = Headers::default();
        for (header_name, header_value) in request.headers.iter() {
            if !filter.contains(header_name.as_str()) {
                continue;
            }
            match headers.entry(header_name.to_string()) {
                Entry::Vacant(e) => {
                    if let Ok(s) = header_value.to_str() {
                        e.insert(HeaderValue::SingleString(s.to_string()));
                    } else {
                        e.insert(HeaderValue::SingleBytes(header_value.as_bytes().to_vec()));
                    }
                }
                Entry::Occupied(mut e) => {
                    if let Ok(s) = header_value.to_str() {
                        e.get_mut().extend_with_string(s.to_string());
                    } else {
                        e.get_mut()
                            .extend_with_bytes(header_value.as_bytes().to_vec());
                    }
                }
            }
        }
        data.headers = Some(headers);
    }
    if vary.cache_buster {
        data.cache_buster = CACHE_BUSTER.fetch_add(1, Ordering::SeqCst);
    }
    Ok(data)
}
