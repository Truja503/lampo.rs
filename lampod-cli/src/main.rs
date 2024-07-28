#[allow(dead_code)]
mod args;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::JoinHandle;

use radicle_term as term;

use lampo_async_jsonrpc::JSONRPCv2;
use lampo_bitcoind::BitcoinCore;
use lampo_common::backend::Backend;
use lampo_common::conf::LampoConf;
use lampo_common::error;
use lampo_common::logger;
use lampo_core_wallet::CoreWalletManager;
use lampod::chain::WalletManager;
use lampod::jsonrpc::channels::json_close_channel;
use lampod::jsonrpc::channels::json_list_channels;
use lampod::jsonrpc::inventory::get_info;
use lampod::jsonrpc::offchain::json_decode_invoice;
use lampod::jsonrpc::offchain::json_invoice;
use lampod::jsonrpc::offchain::json_keysend;
use lampod::jsonrpc::offchain::json_offer;
use lampod::jsonrpc::offchain::json_pay;
use lampod::jsonrpc::onchain::json_estimate_fees;
use lampod::jsonrpc::onchain::json_funds;
use lampod::jsonrpc::onchain::json_new_addr;
use lampod::jsonrpc::open_channel::json_open_channel;
use lampod::jsonrpc::peer_control::json_connect;
use lampod::LampoDaemon;

use crate::args::LampoCliArgs;

#[tokio::main]
async fn main() -> error::Result<()> {
    log::debug!("Started!");
    let args = args::parse_args()?;
    run(args).await?;
    Ok(())
}

/// Return the root directory.
async fn run(args: LampoCliArgs) -> error::Result<()> {
    let mnemonic = if args.restore_wallet {
        let inputs: String = term::input(
            "BIP 39 Mnemonic",
            None,
            Some("To restore the wallet, lampo needs a BIP39 mnemonic with words separated by spaces."),
        )?;
        Some(inputs)
    } else {
        None
    };

    let dev_force_poll = args.dev_force_poll;
    // After this point the configuration is ready!
    let mut lampo_conf: LampoConf = args.try_into()?;
    log::debug!(target: "lampod-cli", "init wallet ..");
    // init the logger here
    logger::init(
        &lampo_conf.log_level,
        lampo_conf
            .log_file
            .as_ref()
            .and_then(|path| Some(PathBuf::from_str(&path).unwrap())),
    )
    .expect("unable to init the logger for the first time");

    lampo_conf
        .ldk_conf
        .channel_handshake_limits
        .force_announced_channel_preference = false;
    // Prepare the backend
    let client = lampo_conf.node.clone();
    log::debug!(target: "lampod-cli", "lampo running with `{client}` backend");
    let client: Arc<dyn Backend> = match client.as_str() {
        "core" => Arc::new(BitcoinCore::new(
            &lampo_conf
                .core_url
                .clone()
                .ok_or(error::anyhow!("Miss the bitcoin url"))?,
            &lampo_conf
                .core_user
                .clone()
                .ok_or(error::anyhow!("Miss the bitcoin user for auth"))?,
            &lampo_conf
                .core_pass
                .clone()
                .ok_or(error::anyhow!("Miss the bitcoin password for auth"))?,
            Arc::new(false),
            // FIXME: allow this under a dev flag
            if dev_force_poll { Some(1) } else { Some(60) },
        )?),
        _ => error::bail!("client {:?} not supported", client),
    };

    let wallet = if let Some(ref priv_key) = lampo_conf.private_key {
        #[cfg(debug_assertions)]
        {
            let Ok(key) = lampo_common::secp256k1::SecretKey::from_str(priv_key) else {
                error::bail!("invalid private key `{priv_key}`");
            };
            let key = lampo_common::bitcoin::PrivateKey::new(key, lampo_conf.network);
            let wallet = CoreWalletManager::try_from((
                key,
                lampo_conf.channels_keys.clone(),
                Arc::new(lampo_conf.clone()),
            ));
            let Ok(wallet) = wallet else {
                error::bail!("error while create the wallet: `{}`", wallet.err().unwrap());
            };
            wallet
        }
        #[cfg(not(debug_assertions))]
        unimplemented!()
    } else if mnemonic.is_none() {
        let (wallet, mnemonic) = match client.kind() {
            lampo_common::backend::BackendKind::Core => {
                CoreWalletManager::new(Arc::new(lampo_conf.clone()))?
            }
            lampo_common::backend::BackendKind::Nakamoto => {
                error::bail!("wallet is not implemented for nakamoto")
            }
        };

        radicle_term::success!("Wallet Generated, please store these words in a safe way");
        radicle_term::println(
            radicle_term::format::badge_primary("wallet-keys"),
            format!("{}", radicle_term::format::highlight(mnemonic)),
        );
        wallet
    } else {
        match client.kind() {
            lampo_common::backend::BackendKind::Core => {
                // SAFETY: It is safe to unwrap the mnemonic because we check it
                // before.
                CoreWalletManager::restore(Arc::new(lampo_conf.clone()), &mnemonic.unwrap())?
            }
            lampo_common::backend::BackendKind::Nakamoto => {
                error::bail!("wallet is not implemented for nakamoto")
            }
        }
    };
    log::debug!(target: "lampod-cli", "wallet created with success");
    let mut lampod = LampoDaemon::new(lampo_conf.clone(), Arc::new(wallet));

    // Init the lampod
    lampod.init(client)?;

    log::debug!(target: "lampod-cli", "Lampo directory `{}`", lampo_conf.path());
    let mut _pid = filelock_rs::pid::Pid::new(lampo_conf.path(), "lampod".to_owned())
        .map_err(|err| {
            log::error!("{err}");
            error::anyhow!("impossible take a lock on the `lampod.pid` file, maybe there is another instance running?")
        })?;

    let lampod = Arc::new(lampod);

    ctrlc::set_handler(move || {
        use std::time::Duration;
        log::info!("Shutdown...");
        std::thread::sleep(Duration::from_secs(1));
        std::process::exit(0);
    })?;

    let workder = lampod.clone().listen()?;
    run_jsonrpc(lampod.clone()).await?;
    log::info!(target: "lampod-cli", "------------ Starting Server ------------");
    log::error!("not blocking");
    workder.join().unwrap();
    Ok(())
}

async fn run_jsonrpc(lampod: Arc<LampoDaemon>) -> error::Result<()> {
    let ws_addr = "127.0.0.1:9999";
    let mut server = JSONRPCv2::new(lampod, ws_addr)?;
    server.add_sync_rpc("getinfo", get_info)?;
    server.add_sync_rpc("connect", json_connect)?;
    server.add_sync_rpc("fundchannel", json_open_channel)?;
    server.add_sync_rpc("newaddr", json_new_addr)?;
    server.add_sync_rpc("channels", json_list_channels)?;
    server.add_sync_rpc("funds", json_funds)?;
    server.add_sync_rpc("invoice", json_invoice)?;
    server.add_sync_rpc("offer", json_offer)?;
    server.add_sync_rpc("decode", json_decode_invoice)?;
    server.add_sync_rpc("pay", json_pay)?;
    server.add_sync_rpc("keysend", json_keysend)?;
    server.add_sync_rpc("fees", json_estimate_fees)?;
    server.add_sync_rpc("close", json_close_channel)?;
    server.listen().await?;
    Ok(())
}
