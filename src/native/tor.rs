// Copyright (c) 2022-2024 Yuki Kishimoto
// Distributed under the MIT software license

//! Tor

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arti_client::config::onion_service::OnionServiceConfigBuilder;
use arti_client::config::{CfgPath, ConfigBuildError, TorClientConfigBuilder};
use arti_client::{DataStream, TorClient, TorClientConfig};
use async_utility::thread;
use tokio::sync::OnceCell;
use tor_hsrproxy::config::{
    Encapsulation, ProxyAction, ProxyConfigBuilder, ProxyConfigError, ProxyPattern, ProxyRule,
    TargetAddr,
};
use tor_hsrproxy::OnionServiceReverseProxy;
use tor_hsservice::{HsNickname, InvalidNickname, OnionServiceConfig, RunningOnionService};
use tor_rtcompat::PreferredRuntime;

static TOR_CLIENT: OnceCell<TorClient<PreferredRuntime>> = OnceCell::const_new();

#[derive(Debug, Clone)]
pub enum Error {
    /// Arti Client error
    ArtiClient(arti_client::Error),
    /// Config builder error
    ConfigBuilder(ConfigBuildError),
    /// Proxy config error
    ProxyConfig(ProxyConfigError),
    /// Invalid nickname
    InvalidNickname(InvalidNickname),
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArtiClient(e) => write!(f, "{e}"),
            Self::ConfigBuilder(e) => write!(f, "{e}"),
            Self::ProxyConfig(e) => write!(f, "{e}"),
            Self::InvalidNickname(e) => write!(f, "{e}"),
        }
    }
}

impl From<arti_client::Error> for Error {
    fn from(e: arti_client::Error) -> Self {
        Self::ArtiClient(e)
    }
}

impl From<ConfigBuildError> for Error {
    fn from(e: ConfigBuildError) -> Self {
        Self::ConfigBuilder(e)
    }
}

impl From<ProxyConfigError> for Error {
    fn from(e: ProxyConfigError) -> Self {
        Self::ProxyConfig(e)
    }
}

impl From<InvalidNickname> for Error {
    fn from(e: InvalidNickname) -> Self {
        Self::InvalidNickname(e)
    }
}

async fn init_tor_client(
    custom_path: Option<&PathBuf>,
) -> Result<TorClient<PreferredRuntime>, Error> {
    // Construct default Tor Client config
    let mut config = TorClientConfigBuilder::default();

    // Enable hidden services
    config.address_filter().allow_onion_addrs(true);

    // Check if is set a custom arti cache path
    if let Some(path) = custom_path {
        let cache: PathBuf = path.join("cache");
        let state: PathBuf = path.join("state");

        let cache_dir: CfgPath = CfgPath::new(cache.to_string_lossy().to_string());
        let state_dir: CfgPath = CfgPath::new(state.to_string_lossy().to_string());

        // Set custom paths
        config.storage().cache_dir(cache_dir).state_dir(state_dir);
    }

    let config: TorClientConfig = config.build()?;
    Ok(TorClient::builder()
        .config(config)
        .create_bootstrapped()
        .await?)
}

/// Get or init tor client
#[inline]
async fn get_tor_client<'a>(
    custom_path: Option<&PathBuf>,
) -> Result<&'a TorClient<PreferredRuntime>, Error> {
    TOR_CLIENT
        .get_or_try_init(|| async { init_tor_client(custom_path).await })
        .await
}

#[inline]
pub(super) async fn connect(
    domain: &str,
    port: u16,
    custom_path: Option<&PathBuf>,
) -> Result<DataStream, Error> {
    let client: &TorClient<PreferredRuntime> = get_tor_client(custom_path).await?;
    Ok(client.connect((domain, port)).await?)
}

/// Launch onion service and forward requests from `hiddenservice.onion:<port>` to [`SocketAddr`].
pub async fn launch_onion_service<S>(
    nickname: S,
    addr: SocketAddr,
    port: u16,
    custom_path: Option<&PathBuf>,
) -> Result<Arc<RunningOnionService>, Error>
where
    S: Into<String>,
{
    // Get tor client
    let client: &TorClient<PreferredRuntime> = get_tor_client(custom_path).await?;

    // Configure proxy
    let mut config: ProxyConfigBuilder = ProxyConfigBuilder::default();
    let pattern: ProxyPattern = ProxyPattern::one_port(port)?;
    let action: ProxyAction =
        ProxyAction::Forward(Encapsulation::default(), TargetAddr::Inet(addr));
    config.set_proxy_ports(vec![ProxyRule::new(pattern, action)]);
    let proxy = OnionServiceReverseProxy::new(config.build()?);

    let nickname: HsNickname = HsNickname::new(nickname.into())?;
    let config: OnionServiceConfig = OnionServiceConfigBuilder::default()
        .nickname(nickname.clone())
        .build()?;

    let (service, stream) = client.launch_onion_service(config)?;

    // TODO: handle error?
    let runtime = client.runtime().clone();
    let _ = thread::spawn(async move {
        proxy
            .handle_requests(runtime, nickname, stream)
            .await
            .unwrap();
    });

    Ok(service)
}
