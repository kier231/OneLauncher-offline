use std::sync::Arc;

use arc_swap::ArcSwap;
use oneclient_events::EventBus;
use reqwest::{ClientBuilder, Response};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::config::NetConfig;
use crate::error::{RequestError, body_snippet};
use crate::request::HttpRequest;
use crate::response::{ResponseExt, ResponseOptions};

const MAX_THROTTLE_RETRIES: u32 = 6;

const MAX_INFLIGHT_REQUESTS: usize = 64;

fn retry_after(response: &Response) -> Option<std::time::Duration> {
    let raw = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    let secs: u64 = raw.parse().ok()?;
    Some(std::time::Duration::from_secs(secs.min(60)))
}

fn backoff_delay(attempt: u32) -> std::time::Duration {
    let base = 500u64.saturating_mul(1 << attempt.min(6));
    let capped = base.min(30_000);
    let jitter = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis() as u64)
        .unwrap_or(0))
        % 250;
    std::time::Duration::from_millis(capped + jitter)
}

fn is_curseforge_host(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    matches!(host.as_str(), "curseforge.com" | "forgecdn.net")
        || host.ends_with(".curseforge.com")
        || host.ends_with(".forgecdn.net")
}

fn apply_curseforge_auth(
    request: &mut reqwest::Request,
    api_key: &str,
) -> Result<(), RequestError> {
    const API_KEY_HEADER: reqwest::header::HeaderName =
        reqwest::header::HeaderName::from_static("x-api-key");

    if !is_curseforge_host(request.url()) {
        return Ok(());
    }

    let headers = request.headers_mut();
    if headers.contains_key(&API_KEY_HEADER) {
        return Ok(());
    }

    let mut value = reqwest::header::HeaderValue::try_from(api_key)?;
    value.set_sensitive(true);
    headers.insert(API_KEY_HEADER, value);

    Ok(())
}

#[derive(Clone)]
pub struct RequestClient {
    /// Used only by `send`, which enforces the version-install allowlist.
    client: reqwest::Client,
    /// Returned by `http()` to legacy/direct callers such as account auth,
    /// skins, status checks, and log upload. It can only reach closed loopback.
    offline_client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    /// Shared across every clone so a settings save is visible to in-flight
    /// handles
    /// `ArcSwap` rather than a lock which would serialise requests
    config: Arc<ArcSwap<NetConfig>>,
}

impl RequestClient {
    pub fn http(&self) -> &reqwest::Client {
        &self.offline_client
    }

    #[must_use]
    pub fn config(&self) -> arc_swap::Guard<Arc<NetConfig>> {
        self.config.load()
    }

    pub fn set_config(&self, config: NetConfig) {
        self.config.store(Arc::new(config));
    }

    pub fn new(config: NetConfig) -> Result<Self, RequestError> {
        let client = client_builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if is_version_install_url(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;

        // Direct HTTP callers never receive the allowlisted client.
        let offline_proxy = reqwest::Proxy::all("http://127.0.0.1:9")?;
        let offline_client = client_builder()
            .proxy(offline_proxy)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            client,
            offline_client,
            semaphore: Arc::new(Semaphore::new(MAX_INFLIGHT_REQUESTS)),
            config: Arc::new(ArcSwap::from_pointee(config)),
        })
    }
}

impl RequestClient {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn send(&self, request: impl Into<HttpRequest>) -> Result<Response, RequestError> {
        let mut request: HttpRequest = request.into();
        if !is_version_install_url(request.request.url()) {
            tracing::debug!(url = %request.request.url(), "request blocked: not a version-install endpoint");
            return Err(RequestError::NetworkDisabled);
        }
        let mut retries = 0;
        let mut throttle_retries = 0u32;

        if oneclient_common::consent::blocks_url(request.request.url().as_str()) {
            let url = request.request.url().to_string();
            tracing::debug!(%url, "request withheld: terms and privacy policy declined");
            return Err(RequestError::ConsentRequired { url });
        }

        apply_curseforge_auth(&mut request.request, &self.config.load().curseforge_api_key)?;

        let cloned_backup = request.request.try_clone();
        let cloneable = cloned_backup.is_some();

        let max_retries = if cloneable {
            request.options.max_retries
        } else {
            0
        };

        let mut active_request = request.request;

        tracing::debug!(
            method = %active_request.method(),
            url = %active_request.url(),
            "dispatching http request"
        );

        let res = loop {
            let permit = if request.options.use_semaphore {
                self.semaphore.acquire().await.ok()
            } else {
                None
            };

            match self.client.execute(active_request).await {
                Ok(response) => {
                    let status = response.status();
                    let retryable = status.as_u16() == 429 || status.is_server_error();

                    if retryable && cloneable && throttle_retries < MAX_THROTTLE_RETRIES {
                        throttle_retries += 1;
                        let delay = retry_after(&response)
                            .unwrap_or_else(|| backoff_delay(throttle_retries));
                        tracing::warn!(
                            status = status.as_u16(),
                            attempt = throttle_retries,
                            delay_ms = delay.as_millis() as u64,
                            url = %response.url(),
                            "rate limited / transient error; backing off"
                        );
                        drop(permit);
                        tokio::time::sleep(delay).await;
                        active_request = cloned_backup.as_ref().unwrap().try_clone().unwrap();
                        continue;
                    }

                    break response;
                }
                Err(err) => {
                    if retries < max_retries {
                        retries += 1;

                        let current_backup = cloned_backup.as_ref().unwrap();

                        active_request = current_backup.try_clone().unwrap();

                        tokio::time::sleep(std::time::Duration::from_millis(500 * retries as u64))
                            .await;

                        continue;
                    }

                    crate::status::note_request_result(false);
                    return Err(RequestError::ReqwestError(err));
                }
            }
        };

        crate::status::note_request_result(true);
        Ok(res)
    }

    #[tracing::instrument(level = "debug", skip(self, request, options, events), fields(dest = %dest.as_ref().display()))]
    pub async fn download_file(
        &self,
        request: impl Into<HttpRequest>,
        dest: impl AsRef<std::path::Path> + Send,
        options: ResponseOptions,
        events: &EventBus,
    ) -> Result<(), RequestError> {
        let res = self.send(request).await?;

        // Without this an error body is written to disk under the requested file's name
        let status = res.status();
        if !status.is_success() {
            let url = res.url().to_string();
            let bytes = res.bytes().await?;
            return Err(RequestError::HttpStatus {
                status: status.as_u16(),
                url,
                snippet: body_snippet(&bytes),
            });
        }

        let size_hint = res.content_length();
        let http_stream = res.stream(options, events).await?;
        let http_stream = std::pin::pin!(http_stream);

        polyio::write_stream(dest, http_stream, size_hint).await?;

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn send_as<T: DeserializeOwned>(&self, request: impl Into<HttpRequest>) -> Result<T, RequestError> {
        let res = self.send(request).await?;
        let status = res.status();
        let url = res.url().to_string();
        let bytes = res.bytes().await?;

        if !status.is_success() {
            return Err(RequestError::HttpStatus {
                status: status.as_u16(),
                url,
                snippet: body_snippet(&bytes),
            });
        }

        serde_json::from_slice(&bytes).map_err(|err| RequestError::DeserializeError {
            source: err,
            type_name: std::any::type_name::<T>().to_string(),
            url,
            status: status.as_u16(),
            snippet: body_snippet(&bytes),
        })
    }

    #[tracing::instrument(level = "debug", skip(self, body, extra_headers), fields(method = %method, %url))]
    pub async fn send_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        body: Option<serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<T, RequestError> {
        let mut request = reqwest::Request::new(method, url);
        for (name, value) in extra_headers {
            request.headers_mut().insert(
                reqwest::header::HeaderName::try_from(*name)?,
                reqwest::header::HeaderValue::try_from(*value)?,
            );
        }

        if let Some(body) = body {
            let bytes = serde_json::to_vec(&body).map_err(RequestError::SerializeError)?;
            request.headers_mut().insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            *request.body_mut() = Some(bytes.into());
        }

        self.send_as(request).await
    }
}

fn client_builder() -> ClientBuilder {
    ClientBuilder::new()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_mins(10))
        .tls_backend_rustls()
        .user_agent(format!(
            "OneClient {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_HOMEPAGE")
        ))
        .no_hickory_dns()
}

fn is_version_install_url(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }

    let Some(host) = url.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase())
    else {
        return false;
    };
    let path = url.path();

    match host.as_str() {
        // Minecraft manifests, client jars, libraries, and assets.
        "piston-meta.mojang.com" | "launchermeta.mojang.com" => {
            path.starts_with("/mc/game/")
        }
        "piston-data.mojang.com" | "launcher.mojang.com" => true,
        "libraries.minecraft.net" | "resources.download.minecraft.net" => true,

        // OneClient's Minecraft/loader manifests and version catalogue only.
        "meta.polyfrost.org" => [
            "/minecraft/", "/forge/", "/neo/", "/fabric/", "/quilt/", "/ornithe/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix)),
        "data-v2.polyfrost.org" => path.starts_with("/oneclient/versions/"),

        // Mod-loader artifacts referenced by the version manifests.
        "maven.minecraftforge.net"
        | "maven.neoforged.net"
        | "maven.fabricmc.net"
        | "maven.quiltmc.org"
        | "maven.ornithemc.net"
        | "maven.legacyfabric.net" => true,

        // Java runtimes required to run an installed Minecraft version.
        "api.azul.com"
        | "cdn.azul.com"
        | "api.bell-sw.com"
        | "download.bell-sw.com"
        | "api.adoptium.net"
        | "github.com"
        | "objects.githubusercontent.com"
        | "release-assets.githubusercontent.com"
        | "corretto.github.io"
        | "corretto.aws"
        | "aka.ms"
        | "download.visualstudio.microsoft.com" => true,

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> reqwest::Request {
        reqwest::Request::new(reqwest::Method::GET, url.parse().unwrap())
    }

    fn api_key_of(url: &str) -> Option<String> {
        let mut req = request(url);
        apply_curseforge_auth(&mut req, oneclient_common::constants::CURSEFORGE_API_KEY).unwrap();
        req.headers()
            .get("x-api-key")
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn attaches_key_to_curseforge_api_and_cdn() {
        for url in [
            "https://api.curseforge.com/v1/mods/search",
            "https://media.forgecdn.net/avatars/1/2/icon.png",
            "https://edge.forgecdn.net/files/1/2/mod.jar",
            "https://www.curseforge.com/minecraft",
            "https://forgecdn.net/thing",
        ] {
            assert_eq!(
                api_key_of(url).as_deref(),
                Some(oneclient_common::constants::CURSEFORGE_API_KEY),
                "expected key on {url}"
            );
        }
    }

    #[test]
    fn ignores_unrelated_and_insecure_hosts() {
        for url in [
            "https://api.modrinth.com/v2/search",
            "https://cdn.modrinth.com/data/x/y.jar",
            "https://notcurseforge.com/x",
            "https://evil-forgecdn.net.attacker.com/x",
            "http://media.forgecdn.net/insecure.png",
        ] {
            assert_eq!(api_key_of(url), None, "unexpected key on {url}");
        }
    }

    #[test]
    fn does_not_override_explicit_key() {
        let mut req = request("https://api.curseforge.com/v1/mods");
        req.headers_mut()
            .insert("x-api-key", reqwest::header::HeaderValue::from_static("mine"));
        apply_curseforge_auth(&mut req, oneclient_common::constants::CURSEFORGE_API_KEY).unwrap();
        assert_eq!(req.headers()["x-api-key"], "mine");
    }

    #[tokio::test]
    async fn non_install_requests_are_rejected_before_dispatch() {
        let client = RequestClient::new(NetConfig::default()).unwrap();
        let error = client
            .send(request("https://example.com/"))
            .await
            .unwrap_err();

        assert!(matches!(error, RequestError::NetworkDisabled));
    }

    #[test]
    fn allows_only_version_install_endpoints() {
        for url in [
            "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
            "https://piston-data.mojang.com/v1/objects/hash/client.jar",
            "https://libraries.minecraft.net/com/mojang/example.jar",
            "https://resources.download.minecraft.net/ab/hash",
            "https://meta.polyfrost.org/minecraft/v0/manifest.json",
            "https://meta.polyfrost.org/fabric/v0/manifest.json",
            "https://data-v2.polyfrost.org/oneclient/versions/metadata.json",
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader.jar",
            "https://api.adoptium.net/v3/assets/version/21",
        ] {
            let url = reqwest::Url::parse(url).unwrap();
            assert!(is_version_install_url(&url), "expected install URL: {url}");
        }

        for url in [
            "http://libraries.minecraft.net/example.jar",
            "https://api.minecraftservices.com/entitlements/mcstore",
            "https://sessionserver.mojang.com/session/minecraft/profile/id",
            "https://textures.minecraft.net/texture/hash",
            "https://api.modrinth.com/v2/search",
            "https://cdn.modrinth.com/data/project/file.jar",
            "https://api.curseforge.com/v1/mods/search",
            "https://api.mclo.gs/1/log",
            "https://plus.polyfrost.org/session",
            "https://polyfrost.org/legal/terms",
            "https://data-v2.polyfrost.org/changelog.json",
            "https://libraries.minecraft.net.attacker.example/file.jar",
        ] {
            let url = reqwest::Url::parse(url).unwrap();
            assert!(!is_version_install_url(&url), "expected blocked URL: {url}");
        }
    }
}
