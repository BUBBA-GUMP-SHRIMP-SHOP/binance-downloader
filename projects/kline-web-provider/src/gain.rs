use anyhow::Context;
use extism::{CompiledPlugin, Manifest, Plugin, PluginBuilder, Wasm};
use rocket::tokio::{sync::OnceCell, task::spawn_blocking};
use serde_json::Number;
use std::{io, time::Duration};

const URL: &str = "https://github.com/BUBBA-GUMP-SHRIMP-SHOP/extism-wasm-plugins/releases/download/v1.0.0/gain-v1.0.0.wasm";
const SHA256: &str = "6262f1237ee59e66f1daf7a0ae2c61e5b9d7fc36a0b46eb6492349278e3eda1c";
static COMPILED: OnceCell<CompiledPlugin> = OnceCell::const_new();

pub(crate) async fn plugin() -> anyhow::Result<Plugin> {
    let compiled = COMPILED
        .get_or_try_init(|| async {
            let wasm = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?
                .get(URL)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let manifest = Manifest::new([Wasm::data(wasm.to_vec()).with_hash(SHA256)]);
            spawn_blocking(move || PluginBuilder::new(manifest).compile()).await?
        })
        .await
        .context("could not load the gain-v1.0.0 Extism plugin")?;
    let plugin = Plugin::new_from_compiled(compiled)?;
    anyhow::ensure!(plugin.function_exists("gain"), "missing gain export");
    Ok(plugin)
}

pub(crate) fn calculate(
    plugin: &mut Plugin,
    former: &Number,
    later: &Number,
) -> io::Result<Option<f64>> {
    let (Some(former), Some(later)) = (former.as_f64(), later.as_f64()) else {
        return Ok(None);
    };
    let input = serde_json::json!({ "former": former, "later": later }).to_string();
    let output: &[u8] = plugin.call("gain", input).map_err(io::Error::other)?;
    serde_json::from_slice(output).map_err(io::Error::other)
}
