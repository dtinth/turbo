//! JSON asset support for turbopack.
//!
//! JSON assets are parsed to ensure they contain valid JSON.
//!
//! When imported from ES modules, they produce a module that exports the
//! JSON value as an object.

#![feature(min_specialization)]

use anyhow::{Context, Result};
use turbo_tasks::{primitives::StringVc, ValueToString, ValueToStringVc};
use turbo_tasks_fs::FileSystemPathVc;
use turbopack_core::{
    asset::{Asset, AssetContentVc, AssetVc},
    chunk::{ChunkItem, ChunkItemVc, ChunkVc, ChunkableAsset, ChunkableAssetVc, ChunkingContextVc},
    reference::AssetReferencesVc,
};
use turbopack_ecmascript::chunk::{
    EcmascriptChunkItem, EcmascriptChunkItemContent, EcmascriptChunkItemContentVc,
    EcmascriptChunkItemVc, EcmascriptChunkPlaceable, EcmascriptChunkPlaceableVc, EcmascriptChunkVc,
};

#[turbo_tasks::value]
pub struct JsonModuleAsset {
    source: AssetVc,
}

#[turbo_tasks::value_impl]
impl JsonModuleAssetVc {
    #[turbo_tasks::function]
    pub fn new(source: AssetVc) -> Self {
        Self::cell(JsonModuleAsset { source })
    }
}

#[turbo_tasks::value_impl]
impl Asset for JsonModuleAsset {
    #[turbo_tasks::function]
    fn path(&self) -> FileSystemPathVc {
        self.source.path()
    }

    #[turbo_tasks::function]
    fn content(&self) -> AssetContentVc {
        self.source.content()
    }
}

#[turbo_tasks::value_impl]
impl ChunkableAsset for JsonModuleAsset {
    #[turbo_tasks::function]
    fn as_chunk(self_vc: JsonModuleAssetVc, context: ChunkingContextVc) -> ChunkVc {
        EcmascriptChunkVc::new(context, self_vc.as_ecmascript_chunk_placeable()).into()
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkPlaceable for JsonModuleAsset {
    #[turbo_tasks::function]
    fn as_chunk_item(
        self_vc: JsonModuleAssetVc,
        context: ChunkingContextVc,
    ) -> EcmascriptChunkItemVc {
        JsonChunkItemVc::cell(JsonChunkItem {
            module: self_vc,
            context,
        })
        .into()
    }
}

#[turbo_tasks::value]
struct JsonChunkItem {
    module: JsonModuleAssetVc,
    context: ChunkingContextVc,
}

#[turbo_tasks::value_impl]
impl ValueToString for JsonChunkItem {
    #[turbo_tasks::function]
    async fn to_string(&self) -> Result<StringVc> {
        Ok(StringVc::cell(format!(
            "{} (json)",
            self.module.await?.source.path().to_string().await?
        )))
    }
}

#[turbo_tasks::value_impl]
impl ChunkItem for JsonChunkItem {
    #[turbo_tasks::function]
    fn references(&self) -> AssetReferencesVc {
        self.module.references()
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkItem for JsonChunkItem {
    #[turbo_tasks::function]
    fn chunking_context(&self) -> ChunkingContextVc {
        self.context
    }

    #[turbo_tasks::function]
    fn related_path(&self) -> FileSystemPathVc {
        self.module.path()
    }

    #[turbo_tasks::function]
    async fn content(&self) -> Result<EcmascriptChunkItemContentVc> {
        // We parse to JSON and then stringify again to ensure that the
        // JSON is valid.
        let content = self
            .module
            .path()
            .read_json()
            .to_string()
            .await
            .context("Unable to make a module from invalid JSON")?;
        let js_str_content = serde_json::to_string(content.as_str())?;
        let inner_code = format!("__turbopack_export_value__(JSON.parse({js_str_content}));");
        Ok(EcmascriptChunkItemContent {
            inner_code: inner_code.into(),
            ..Default::default()
        }
        .into())
    }
}

pub fn register() {
    turbo_tasks::register();
    turbo_tasks_fs::register();
    turbopack_core::register();
    turbopack_ecmascript::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}
