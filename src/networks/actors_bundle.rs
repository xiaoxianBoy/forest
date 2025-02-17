// Copyright 2019-2024 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use std::io::{self, Cursor};
use std::path::Path;

use anyhow::{ensure, Context as _};
use async_compression::tokio::write::ZstdEncoder;
use cid::Cid;
use futures::stream::FuturesUnordered;
use futures::{stream, StreamExt, TryStreamExt};
use itertools::Itertools;
use nonempty::NonEmpty;
use once_cell::sync::Lazy;
use reqwest::Url;
use tokio::fs::File;
use tracing::warn;

use crate::utils::db::car_stream::{CarStream, CarWriter};
use crate::utils::net::http_get;

use std::str::FromStr;

use super::NetworkChain;

#[derive(Debug)]
pub struct ActorBundleInfo {
    pub manifest: Cid,
    pub url: Url,
    /// Alternative URL to download the bundle from if the primary URL fails.
    /// Note that we host the bundles and so we need to update the bucket
    /// ourselves when a new bundle is released.
    pub alt_url: Url,
    pub network: NetworkChain,
}

macro_rules! actor_bundle_info {
    ($($cid:literal @ $version:literal for $network:literal),* $(,)?) => {
        [
            $(
                ActorBundleInfo {
                    manifest: $cid.parse().unwrap(),
                    url: concat!(
                            "https://github.com/filecoin-project/builtin-actors/releases/download/",
                            $version,
                            "/builtin-actors-",
                            $network,
                            ".car"
                        ).parse().unwrap(),
                    alt_url: concat!(
                          "https://filecoin-actors.chainsafe.dev/",
                          $version,
                            "/builtin-actors-",
                            $network,
                            ".car"
                        ).parse().unwrap(),
                    network: NetworkChain::from_str($network).unwrap(),
                },
            )*
        ]
    }
}

pub static ACTOR_BUNDLES: Lazy<Box<[ActorBundleInfo]>> = Lazy::new(|| {
    Box::new(actor_bundle_info![
        "bafy2bzacedbedgynklc4dgpyxippkxmba2mgtw7ecntoneclsvvl4klqwuyyy" @ "v9.0.3" for "calibrationnet",
        "bafy2bzaced25ta3j6ygs34roprilbtb3f6mxifyfnm7z7ndquaruxzdq3y7lo" @ "v10.0.0-rc.1" for "calibrationnet",
        "bafy2bzacedhuowetjy2h4cxnijz2l64h4mzpk5m256oywp4evarpono3cjhco" @ "v11.0.0-rc2" for "calibrationnet",
        "bafy2bzacedrunxfqta5skb7q7x32lnp4efz2oq7fn226ffm7fu5iqs62jkmvs" @ "v12.0.0-rc.1" for "calibrationnet",
        "bafy2bzacebl4w5ptfvuw6746w7ev562idkbf5ppq72e6zub22435ws2rukzru" @ "v12.0.0-rc.2" for "calibrationnet",
        "bafy2bzacednzb3pkrfnbfhmoqtb3bc6dgvxszpqklf3qcc7qzcage4ewzxsca" @ "v12.0.0" for "calibrationnet",
        "bafy2bzacea4firkyvt2zzdwqjrws5pyeluaesh6uaid246tommayr4337xpmi" @ "v13.0.0-rc.3" for "calibrationnet",
        "bafy2bzacectxvbk77ntedhztd6sszp2btrtvsmy7lp2ypnrk6yl74zb34t2cq" @ "v12.0.0" for "butterflynet",
        "bafy2bzaceaqx5xa4cwso24rjiu2ketjlztrqlac6dkyol7tlyuhzrle3zfbos" @ "v13.0.0-rc.3" for "butterflynet",
        "bafy2bzacedozk3jh2j4nobqotkbofodq4chbrabioxbfrygpldgoxs3zwgggk" @ "v9.0.3" for "devnet",
        "bafy2bzacebzz376j5kizfck56366kdz5aut6ktqrvqbi3efa2d4l2o2m653ts" @ "v10.0.0" for "devnet",
        "bafy2bzaceay35go4xbjb45km6o46e5bib3bi46panhovcbedrynzwmm3drr4i" @ "v11.0.0" for "devnet",
        "bafy2bzaceasjdukhhyjbegpli247vbf5h64f7uvxhhebdihuqsj2mwisdwa6o" @ "v12.0.0" for "devnet",
        "bafy2bzacecn7uxgehrqbcs462ktl2h23u23cmduy2etqj6xrd6tkkja56fna4" @ "v13.0.0" for "devnet",
        "bafy2bzaceb6j6666h36xnhksu3ww4kxb6e25niayfgkdnifaqi6m6ooc66i6i" @ "v9.0.3" for "mainnet",
        "bafy2bzacecsuyf7mmvrhkx2evng5gnz5canlnz2fdlzu2lvcgptiq2pzuovos" @ "v10.0.0" for "mainnet",
        "bafy2bzacecnhaiwcrpyjvzl4uv4q3jzoif26okl3m66q3cijp3dfwlcxwztwo" @ "v11.0.0" for "mainnet",
        "bafy2bzaceapkgfggvxyllnmuogtwasmsv5qi2qzhc2aybockd6kag2g5lzaio" @ "v12.0.0" for "mainnet",
        "bafy2bzacecdhvfmtirtojwhw2tyciu4jkbpsbk5g53oe24br27oy62sn4dc4e" @ "v13.0.0" for "mainnet",
    ])
});

pub async fn generate_actor_bundle(output: &Path) -> anyhow::Result<()> {
    let (mut roots, blocks) = FuturesUnordered::from_iter(ACTOR_BUNDLES.iter().map(
        |ActorBundleInfo {
             manifest: root,
             url,
             alt_url,
             network: _,
         }| async move {
            let response = if let Ok(response) = http_get(url).await {
                response
            } else {
                warn!("failed to download bundle from primary URL, trying alternative URL");
                http_get(alt_url).await?
            };
            let bytes = response.bytes().await?;
            let car = CarStream::new(Cursor::new(bytes)).await?;
            ensure!(car.header.version == 1);
            ensure!(car.header.roots.len() == 1);
            ensure!(car.header.roots.first() == root);
            anyhow::Ok((*root, car.try_collect::<Vec<_>>().await?))
        },
    ))
    .try_collect::<Vec<_>>()
    .await?
    .into_iter()
    .unzip::<_, _, Vec<_>, Vec<_>>();

    ensure!(roots.iter().all_unique());

    roots.sort(); // deterministic

    let mut blocks = blocks.into_iter().flatten().collect::<Vec<_>>();
    blocks.sort();
    blocks.dedup();

    for block in blocks.iter() {
        ensure!(
            block.valid(),
            "sources contain an invalid block, cid {}",
            block.cid
        )
    }

    stream::iter(blocks)
        .map(io::Result::Ok)
        .forward(CarWriter::new_carv1(
            NonEmpty::from_vec(roots).context("car roots cannot be empty")?,
            ZstdEncoder::with_quality(
                File::create(&output).await?,
                async_compression::Level::Precise(17),
            ),
        )?)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use http0::StatusCode;
    use reqwest::Response;
    use std::time::Duration;

    use crate::utils::net::global_http_client;

    use super::*;

    #[tokio::test]
    async fn check_bundles_are_mirrored() {
        // Run the test only in CI so that regular test on dev machines don't download the bundles
        // on poor internet connections.
        if std::env::var("CI").is_err() {
            return;
        }

        FuturesUnordered::from_iter(ACTOR_BUNDLES.iter().map(
            |ActorBundleInfo {
                 manifest,
                 url,
                 alt_url,
                 network: _,
             }| async move {
                let (primary, alt) = match (http_get(url).await, http_get(alt_url).await) {
                    (Ok(primary), Ok(alt)) => (primary, alt),
                    (Err(_), Err(_)) => anyhow::bail!("Both sources are down"),
                    // If either of the sources are otherwise down, we don't want to fail the test.
                    _ => return anyhow::Ok(()),
                };

                // Check that neither of the sources respond with 404.
                // Such code would indicate that the bundle URLs are incorrect.
                // In case of GH releases, it may have been yanked for some reason.
                // In case of our own bundles, it may have been not uploaded (or deleted).
                assert_ne!(
                    StatusCode::NOT_FOUND,
                    primary.status(),
                    "Could not download {url}"
                );
                assert_ne!(
                    StatusCode::NOT_FOUND,
                    alt.status(),
                    "Could not download {alt_url}"
                );

                // If either of the sources are otherwise down, we don't want to fail the test.
                // This is because we don't want to fail the test if the infrastructure is down.
                if !primary.status().is_success() || !alt.status().is_success() {
                    return anyhow::Ok(());
                }

                // Check that the bundles are identical.
                // This is to ensure that the bundle was not tamperered with and that the
                // bundle was uploaded to the alternative URL correctly.
                let (primary, alt) = match (primary.bytes().await, alt.bytes().await) {
                    (Ok(primary), Ok(alt)) => (primary, alt),
                    (Err(_), Err(_)) => anyhow::bail!("Both sources are down"),
                    // If either of the sources are otherwise down, we don't want to fail the test.
                    _ => return anyhow::Ok(()),
                };

                let car_primary = CarStream::new(Cursor::new(primary)).await?;
                let car_secondary = CarStream::new(Cursor::new(alt)).await?;

                assert_eq!(
                    car_primary.header.roots, car_secondary.header.roots,
                    "Roots for {url} and {alt_url} do not match"
                );
                assert_eq!(
                    car_primary.header.roots.first(),
                    manifest,
                    "Manifest for {url} and {alt_url} does not match"
                );

                Ok(())
            },
        ))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    }

    pub async fn http_get(url: &Url) -> anyhow::Result<Response> {
        Ok(global_http_client()
            .get(url.clone())
            .timeout(Duration::from_secs(120))
            .send()
            .await?)
    }
}
