use std::{path::PathBuf, sync::Arc};

use cb_greenfield::{
    api_gateway::{GatewayState, serve},
    config::PlatformConfig,
    observability::Observability,
    pbs_engine::PbsEngine,
    relay::ReqwestRelayTransport,
    signer_engine::SignerEngine,
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "commit-boost-greenfield")]
struct Cli {
    /// Optional path to config TOML file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Optional bind-address override for the unified gateway server.
    #[arg(long)]
    bind: Option<String>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let mut config = if let Some(path) = cli.config {
        PlatformConfig::from_toml_file(path)?
    } else {
        PlatformConfig::default()
    };

    if let Some(bind_override) = cli.bind {
        config.pbs.bind_address = bind_override;
    }

    config.validate_local_invariants()?;

    let metrics = Observability::default();
    let transport = Arc::new(ReqwestRelayTransport::new());
    let pbs = Arc::new(PbsEngine::new(config.pbs.clone(), transport, metrics));
    let signer = Arc::new(SignerEngine::new(vec!["0xgreenfield".to_string()]));

    let state = GatewayState { config: Arc::new(config.clone()), pbs, signer };

    let bind_address = config.pbs.bind_address.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = serve(state, &bind_address, async move {
        let _ = shutdown_rx.await;
    })
    .await?;

    tracing::info!(address = %handle.address, "commit-boost greenfield gateway listening");
    tokio::signal::ctrl_c().await?;
    let _ = shutdown_tx.send(());

    Ok(())
}
