use hbb_common::{
    async_recursion::async_recursion,
    bail,
    config::{Config, Socks5Server},
    dns_fallback::Fallback,
    log::{self, info},
    proxy::{Proxy, ProxyScheme},
    tls::{
        get_cached_tls_accept_invalid_cert, get_cached_tls_type, is_plain, upsert_tls_cache,
        TlsType,
    },
    ResultType,
};
use reqwest::{
    blocking::Client as SyncClient,
    dns::{Addrs, Name, Resolve, Resolving},
    Client as AsyncClient,
};
use std::sync::Arc;

/// Resolves names as `dns-fallback` has it: a name the system cannot resolve gets the address
/// given for it there.
struct FallbackResolver(Arc<Fallback>);

impl Resolve for FallbackResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let fallback = self.0.clone();
        // Port 0: reqwest puts the URL's port in.
        Box::pin(async move {
            let addrs = fallback.lookup(name.as_str(), 0).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The resolver for a client that checks certificates, if `dns-fallback` is set. Only for those:
/// the address it gives has to prove it serves the name.
fn fallback_resolver(
    tls_type: TlsType,
    danger_accept_invalid_cert: bool,
) -> Option<Arc<FallbackResolver>> {
    if !checks_certificate(tls_type, danger_accept_invalid_cert) {
        return None;
    }
    let fallback = Fallback::from_config();
    (!fallback.is_empty()).then(|| Arc::new(FallbackResolver(Arc::new(fallback))))
}

/// The redirects a client with `dns-fallback` follows: as many as reqwest does by default, but
/// none to `http://`, where the fallback address would be taken on trust. The response to such a
/// redirect is returned as it is.
fn stay_on_https(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    if attempt.previous().len() > 10 {
        attempt.error("too many redirects")
    } else if attempt.url().scheme() != "https" {
        attempt.stop()
    } else {
        attempt.follow()
    }
}

/// `Plain` is what an `http://` URL gets (`get_cached_tls_type`).
fn checks_certificate(tls_type: TlsType, danger_accept_invalid_cert: bool) -> bool {
    !matches!(tls_type, TlsType::Plain) && !danger_accept_invalid_cert
}

macro_rules! configure_http_client {
    ($builder:expr, $tls_type:expr, $danger_accept_invalid_cert:expr, $Client: ty) => {{
        // https://github.com/rustdesk/rustdesk/issues/11569
        // https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.no_proxy
        let mut builder = $builder.no_proxy();

        match $tls_type {
            TlsType::Plain => {}
            TlsType::NativeTls => {
                builder = builder.use_native_tls();
                if $danger_accept_invalid_cert {
                    builder = builder.danger_accept_invalid_certs(true);
                }
            }
            TlsType::Rustls => {
                #[cfg(any(target_os = "android", target_os = "ios"))]
                match hbb_common::verifier::client_config($danger_accept_invalid_cert) {
                    Ok(client_config) => {
                        builder = builder.use_preconfigured_tls(client_config);
                    }
                    Err(e) => {
                        hbb_common::log::error!("Failed to get client config: {}", e);
                    }
                }
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                {
                    builder = builder.use_rustls_tls();
                    if $danger_accept_invalid_cert {
                        builder = builder.danger_accept_invalid_certs(true);
                    }
                }
            }
        }

        if let Some(resolver) = fallback_resolver($tls_type, $danger_accept_invalid_cert) {
            builder = builder
                .dns_resolver(resolver)
                .redirect(reqwest::redirect::Policy::custom(stay_on_https));
        }

        let client = if let Some(conf) = Config::get_socks() {
            let proxy_result = Proxy::from_conf(&conf, None);

            match proxy_result {
                Ok(proxy) => {
                    let proxy_setup = match &proxy.intercept {
                        ProxyScheme::Http { host, .. } => {
                            reqwest::Proxy::all(format!("http://{}", host))
                        }
                        ProxyScheme::Https { host, .. } => {
                            reqwest::Proxy::all(format!("https://{}", host))
                        }
                        ProxyScheme::Socks5 { addr, .. } => {
                            reqwest::Proxy::all(&format!("socks5://{}", addr))
                        }
                    };

                    match proxy_setup {
                        Ok(mut p) => {
                            if let Some(auth) = proxy.intercept.maybe_auth() {
                                if !auth.username().is_empty() && !auth.password().is_empty() {
                                    p = p.basic_auth(auth.username(), auth.password());
                                }
                            }
                            builder = builder.proxy(p);
                            builder.build().unwrap_or_else(|e| {
                                info!("Failed to create a proxied client: {}", e);
                                <$Client>::new()
                            })
                        }
                        Err(e) => {
                            info!("Failed to set up proxy: {}", e);
                            <$Client>::new()
                        }
                    }
                }
                Err(e) => {
                    info!("Failed to configure proxy: {}", e);
                    <$Client>::new()
                }
            }
        } else {
            builder.build().unwrap_or_else(|e| {
                info!("Failed to create a client: {}", e);
                <$Client>::new()
            })
        };

        client
    }};
}

pub fn create_http_client(tls_type: TlsType, danger_accept_invalid_cert: bool) -> SyncClient {
    let builder = SyncClient::builder();
    configure_http_client!(builder, tls_type, danger_accept_invalid_cert, SyncClient)
}

pub fn create_http_client_async(
    tls_type: TlsType,
    danger_accept_invalid_cert: bool,
) -> AsyncClient {
    let builder = AsyncClient::builder();
    configure_http_client!(builder, tls_type, danger_accept_invalid_cert, AsyncClient)
}

pub fn get_url_for_tls<'a>(url: &'a str, proxy_conf: &'a Option<Socks5Server>) -> &'a str {
    if is_plain(url) {
        if let Some(conf) = proxy_conf {
            if conf.proxy.starts_with("https://") {
                return &conf.proxy;
            }
        }
    }
    url
}

pub fn create_http_client_with_url(url: &str) -> SyncClient {
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(url, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url);
    let is_tls_type_cached = tls_type.is_some();
    let tls_type = tls_type.unwrap_or(TlsType::Rustls);
    let tls_danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(tls_url);
    create_http_client_with_url_(
        url,
        tls_url,
        tls_type,
        is_tls_type_cached,
        tls_danger_accept_invalid_cert,
        tls_danger_accept_invalid_cert,
    )
}

pub fn create_http_client_with_url_strict(url: &str) -> ResultType<SyncClient> {
    let parsed_url = url::Url::parse(url)?;
    if parsed_url.scheme() != "https" {
        bail!("Strict HTTP client requires HTTPS: {}", url);
    }
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(url, &proxy_conf);
    let cached_tls_type = get_cached_tls_type(tls_url);
    let cached_danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(tls_url);
    let can_reuse_cached_probe =
        cached_tls_type.is_some() && cached_danger_accept_invalid_cert == Some(false);
    let tls_type = if can_reuse_cached_probe {
        cached_tls_type.unwrap_or(TlsType::Rustls)
    } else {
        TlsType::Rustls
    };
    Ok(create_http_client_with_url_(
        url,
        tls_url,
        tls_type,
        can_reuse_cached_probe,
        Some(false),
        Some(false),
    ))
}

fn create_http_client_with_url_(
    url: &str,
    tls_url: &str,
    tls_type: TlsType,
    is_tls_type_cached: bool,
    danger_accept_invalid_cert: Option<bool>,
    original_danger_accept_invalid_cert: Option<bool>,
) -> SyncClient {
    let mut client = create_http_client(tls_type, danger_accept_invalid_cert.unwrap_or(false));
    if is_tls_type_cached && original_danger_accept_invalid_cert.is_some() {
        return client;
    }
    if let Err(e) = client.head(url).send() {
        if e.is_request() {
            match (tls_type, is_tls_type_cached, danger_accept_invalid_cert) {
                (TlsType::Rustls, _, None) => {
                    log::warn!(
                        "Failed to connect to server {} with rustls-tls: {:?}, trying accept invalid cert",
                        tls_url,
                        e
                    );
                    client = create_http_client_with_url_(
                        url,
                        tls_url,
                        tls_type,
                        is_tls_type_cached,
                        Some(true),
                        original_danger_accept_invalid_cert,
                    );
                }
                (TlsType::Rustls, false, Some(_)) => {
                    log::warn!(
                        "Failed to connect to server {} with rustls-tls: {:?}, trying native-tls",
                        tls_url,
                        e
                    );
                    client = create_http_client_with_url_(
                        url,
                        tls_url,
                        TlsType::NativeTls,
                        is_tls_type_cached,
                        original_danger_accept_invalid_cert,
                        original_danger_accept_invalid_cert,
                    );
                }
                (TlsType::NativeTls, _, None) => {
                    log::warn!(
                        "Failed to connect to server {} with native-tls: {:?}, trying accept invalid cert",
                        tls_url,
                        e
                    );
                    client = create_http_client_with_url_(
                        url,
                        tls_url,
                        tls_type,
                        is_tls_type_cached,
                        Some(true),
                        original_danger_accept_invalid_cert,
                    );
                }
                _ => {
                    log::error!(
                        "Failed to connect to server {} with {:?}, err: {:?}.",
                        tls_url,
                        tls_type,
                        e
                    );
                }
            }
        } else {
            log::warn!(
                "Failed to connect to server {} with {:?}, err: {}.",
                tls_url,
                tls_type,
                e
            );
        }
    } else {
        log::info!(
            "Successfully connected to server {} with {:?}",
            tls_url,
            tls_type
        );
        upsert_tls_cache(
            tls_url,
            tls_type,
            danger_accept_invalid_cert.unwrap_or(false),
        );
    }
    client
}

pub async fn create_http_client_async_with_url(url: &str) -> AsyncClient {
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(url, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url);
    let is_tls_type_cached = tls_type.is_some();
    let tls_type = tls_type.unwrap_or(TlsType::Rustls);
    let danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(tls_url);
    create_http_client_async_with_url_(
        url,
        tls_url,
        tls_type,
        is_tls_type_cached,
        danger_accept_invalid_cert,
        danger_accept_invalid_cert,
    )
    .await
}

pub async fn create_http_client_async_with_url_strict(url: &str) -> ResultType<AsyncClient> {
    let parsed_url = url::Url::parse(url)?;
    if parsed_url.scheme() != "https" {
        bail!("Strict HTTP client requires HTTPS: {}", url);
    }
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(url, &proxy_conf);
    let cached_tls_type = get_cached_tls_type(tls_url);
    let cached_danger_accept_invalid_cert = get_cached_tls_accept_invalid_cert(tls_url);
    let can_reuse_cached_probe =
        cached_tls_type.is_some() && cached_danger_accept_invalid_cert == Some(false);
    let tls_type = if can_reuse_cached_probe {
        cached_tls_type.unwrap_or(TlsType::Rustls)
    } else {
        TlsType::Rustls
    };
    Ok(create_http_client_async_with_url_(
        url,
        tls_url,
        tls_type,
        can_reuse_cached_probe,
        Some(false),
        Some(false),
    )
    .await)
}

#[async_recursion]
async fn create_http_client_async_with_url_(
    url: &str,
    tls_url: &str,
    tls_type: TlsType,
    is_tls_type_cached: bool,
    danger_accept_invalid_cert: Option<bool>,
    original_danger_accept_invalid_cert: Option<bool>,
) -> AsyncClient {
    let mut client =
        create_http_client_async(tls_type, danger_accept_invalid_cert.unwrap_or(false));
    if is_tls_type_cached && original_danger_accept_invalid_cert.is_some() {
        return client;
    }
    if let Err(e) = client.head(url).send().await {
        match (tls_type, is_tls_type_cached, danger_accept_invalid_cert) {
            (TlsType::Rustls, _, None) => {
                log::warn!(
                    "Failed to connect to server {} with rustls-tls: {:?}, trying accept invalid cert",
                    tls_url,
                    e
                );
                client = create_http_client_async_with_url_(
                    url,
                    tls_url,
                    tls_type,
                    is_tls_type_cached,
                    Some(true),
                    original_danger_accept_invalid_cert,
                )
                .await;
            }
            (TlsType::Rustls, false, Some(_)) => {
                log::warn!(
                    "Failed to connect to server {} with rustls-tls: {:?}, trying native-tls",
                    tls_url,
                    e
                );
                client = create_http_client_async_with_url_(
                    url,
                    tls_url,
                    TlsType::NativeTls,
                    is_tls_type_cached,
                    original_danger_accept_invalid_cert,
                    original_danger_accept_invalid_cert,
                )
                .await;
            }
            (TlsType::NativeTls, _, None) => {
                log::warn!(
                    "Failed to connect to server {} with native-tls: {:?}, trying accept invalid cert",
                    tls_url,
                    e
                );
                client = create_http_client_async_with_url_(
                    url,
                    tls_url,
                    tls_type,
                    is_tls_type_cached,
                    Some(true),
                    original_danger_accept_invalid_cert,
                )
                .await;
            }
            _ => {
                log::error!(
                    "Failed to connect to server {} with {:?}, err: {:?}.",
                    tls_url,
                    tls_type,
                    e
                );
            }
        }
    } else {
        log::info!(
            "Successfully connected to server {} with {:?}",
            tls_url,
            tls_type
        );
        upsert_tls_cache(
            tls_url,
            tls_type,
            danger_accept_invalid_cert.unwrap_or(false),
        );
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn dns_fallback_only_where_the_certificate_is_checked() {
        assert!(checks_certificate(TlsType::Rustls, false));
        assert!(checks_certificate(TlsType::NativeTls, false));
        assert!(!checks_certificate(TlsType::Rustls, true));
        assert!(!checks_certificate(TlsType::Plain, false));
    }

    /// A server on this machine, one request to a connection: 204 to anything but `/redirect`,
    /// which it sends on to where `redirect_to` says for its port. Returns that port.
    fn serve(redirect_to: impl Fn(u16) -> String) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nConnection: close\r\n\r\n",
            redirect_to(port)
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0; 1024];
                let read = stream.read(&mut request).unwrap_or(0);
                let _ = if request[..read].starts_with(b"GET /redirect") {
                    stream.write_all(redirect.as_bytes())
                } else {
                    stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                };
            }
        });
        port
    }

    /// With `dns-fallback` a redirect to `http://` is not followed, where the address would be
    /// taken on trust: the redirect itself is the response.
    #[test]
    fn redirects_to_http_are_not_followed() {
        let port = serve(|port| format!("http://127.0.0.1:{port}/"));
        let url = format!("http://127.0.0.1:{port}/redirect");
        let followed = SyncClient::builder().no_proxy().build().unwrap();
        assert_eq!(followed.get(&url).send().unwrap().status(), 204);
        let policy = reqwest::redirect::Policy::custom(stay_on_https);
        let kept = SyncClient::builder()
            .no_proxy()
            .redirect(policy)
            .build()
            .unwrap();
        assert_eq!(kept.get(&url).send().unwrap().status(), 302);
    }

    /// Both kinds of client reach a name only `dns-fallback` resolves, at the address given for
    /// it and the URL's port. Over plain HTTP here, which the resolver itself knows nothing of.
    #[test]
    fn clients_reach_a_name_only_the_fallback_resolves() {
        // `.invalid` never resolves (RFC 6761), unless a TUN proxy answers every name.
        let name = "dns-fallback-http.invalid";
        if std::net::ToSocketAddrs::to_socket_addrs(&(name, 0)).is_ok() {
            eprintln!("skipped: this machine's resolver answers for {name}");
            return;
        }
        let url = format!("http://{name}:{}/", serve(|_| String::new()));
        let fallback = Fallback::parse(&format!("{name}=127.0.0.1"));
        let resolver = Arc::new(FallbackResolver(Arc::new(fallback)));

        let sync = SyncClient::builder()
            .no_proxy()
            .dns_resolver(resolver.clone());
        let response = sync.build().unwrap().get(&url).send().unwrap();
        assert_eq!(response.status(), 204);

        let client = AsyncClient::builder().no_proxy().dns_resolver(resolver);
        let response = hbb_common::tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(client.build().unwrap().get(&url).send())
            .unwrap();
        assert_eq!(response.status(), 204);
    }
}
