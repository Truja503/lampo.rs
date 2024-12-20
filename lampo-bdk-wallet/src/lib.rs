//! Wallet Manager implementation with BDK
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use bdk::bitcoin::consensus::serialize;
use bdk::bitcoin::{Amount, ScriptBuf};
use bdk::keys::bip39::{Language, Mnemonic, WordCount};
use bdk::keys::GeneratableKey;
use bdk::keys::{DerivableKey, ExtendedKey, GeneratedKey};
use bdk::template::Bip84;
use bdk::blockchain::esplora::EsploraBlockchain;
use bdk::{Wallet, KeychainKind};
use bdk::SignOptions;
use bdk::FeeRate;
use bdk::wallet::AddressIndex;
use bdk::database::MemoryDatabase;
use bdk_chain::spk_client::FullScanRequestBuilder;
use bdk_esplora::EsploraExt;

use lampo_common::bitcoin::consensus::deserialize;
use lampo_common::bitcoin::{PrivateKey, Transaction};
use lampo_common::conf::{LampoConf, Network};
use lampo_common::error;
use lampo_common::keys::LampoKeys;
use lampo_common::model::response::{NewAddress, Utxo};
use lampo_common::wallet::WalletManager;

pub struct BDKWalletManager {
    pub wallet:  RefCell<Mutex<Wallet<MemoryDatabase>>>,//solve RefCell<Mutex<Wallet>>  
    pub keymanager: Arc<LampoKeys>,
    pub network: Network,
}
// SAFETY: It is safe to do because the `LampoWalletManager`
// is not send and sync due the RefCell, but we use the Mutex
// inside, so we are safe to share across threads.
unsafe impl Send for BDKWalletManager {}
unsafe impl Sync for BDKWalletManager {}

impl BDKWalletManager {
    /// from mnemonic_words build or bkd::Wallet or return an bdk::Error
    fn build_wallet(
        conf: Arc<LampoConf>,
        mnemonic_words: &str,
    ) -> error::Result<(Wallet<MemoryDatabase>, LampoKeys)> {
        // Parse a mnemonic
        let mnemonic = Mnemonic::parse(mnemonic_words)?;
        // Generate the extended key
        let xkey: ExtendedKey = mnemonic.into_extended_key()?;
        let network = match conf.network.to_string().as_str() {
            "bitcoin" => bdk::bitcoin::Network::Bitcoin,
            "testnet" => bdk::bitcoin::Network::Testnet,
            "signet" => bdk::bitcoin::Network::Signet,
            "regtest" => bdk::bitcoin::Network::Regtest,
            _ => unreachable!(),
        };
        // Get xprv from the extended key
        let xprv = xkey
            .into_xprv(network)
            .ok_or(error::anyhow!("wrong convertion to a private key"))?;
       
        let ldk_kesy = LampoKeys::new(xprv.private_key.secret_bytes());
         let db = MemoryDatabase::new();       
        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")
        let wallet = Wallet::new(
            Bip84(xprv, KeychainKind::External),
            Some(Bip84(xprv, KeychainKind::Internal)),
            network,
            db,
        )?;
        let descriptor = wallet.public_descriptor(KeychainKind::Internal).unwrap();
        log::info!("descriptor: {:?}", descriptor);
        Ok((wallet, ldk_kesy))
    }

    #[cfg(debug_assertions)]
    fn build_from_private_key(
        xprv: PrivateKey,
        channel_keys: Option<String>,
    ) -> error::Result<(Wallet<MemoryDatabase>, LampoKeys)> {
        use bdk::bitcoin::bip32::ExtendedPrivKey;

        let ldk_keys = if channel_keys.is_some() {
            LampoKeys::with_channel_keys(xprv.inner.secret_bytes(), channel_keys.unwrap())
        } else {
            LampoKeys::new(xprv.inner.secret_bytes())
        };

        // FIXME: Get a tmp path
         let db = MemoryDatabase::new();   

        let network = match xprv.network.to_string().as_str() {
            "bitcoin" => bdk::bitcoin::Network::Bitcoin,
            "testnet" => bdk::bitcoin::Network::Testnet,
            "signet" => bdk::bitcoin::Network::Signet,
            "regtest" => bdk::bitcoin::Network::Regtest,
            _ => unreachable!(),
        };
        let xprv = ExtendedPrivKey::new_master(network, &[0u8; 32])?; 
        let external_key = ExtendedKey::Private((xprv, std::marker::PhantomData));
        let internal_key = ExtendedKey::Private((xprv, std::marker::PhantomData));
        
        let wallet = Wallet::new(
            Bip84(external_key, KeychainKind::External),
            Some(Bip84(internal_key, KeychainKind::Internal)),
            network,
            db,
        )?;
        Ok((wallet, ldk_keys))
    }
}

impl WalletManager for BDKWalletManager {
    fn new(conf: Arc<LampoConf>) -> error::Result<(Self, String)> {
        // Generate fresh mnemonic
        let mnemonic: GeneratedKey<_, bdk::miniscript::Tap> =
            Mnemonic::generate((WordCount::Words12, Language::English))
                .map_err(|e| error::anyhow!("{:?}", e))?;
        // Convert mnemonic to string
        let mnemonic_words = mnemonic.to_string();
        log::info!("mnemonic words `{mnemonic_words}`");
        let (wallet, keymanager) = BDKWalletManager::build_wallet(conf.clone(), &mnemonic_words)?;
        Ok((
            Self {
                wallet: RefCell::new(Mutex::new(wallet)),
                keymanager: Arc::new(keymanager),
                network: conf.network,
            },
            mnemonic_words,
        ))
    }

    fn restore(conf: Arc<LampoConf>, mnemonic_words: &str) -> error::Result<Self> {
        let (wallet, keymanager) = BDKWalletManager::build_wallet(conf.clone(), mnemonic_words)?;
        Ok(Self {
            wallet: RefCell::new(Mutex::new(wallet)),
            keymanager: Arc::new(keymanager),
            network: conf.network,
        })
    }

    fn ldk_keys(&self) -> Arc<LampoKeys> {
        self.keymanager.clone()
    }

    fn get_onchain_address(&self) -> error::Result<NewAddress> {
        let address = self
            .wallet
            .borrow_mut()
            .lock()
            .unwrap()
            .get_address(bdk::wallet::AddressIndex::New);
        match address{
            Ok(info) => Ok(NewAddress {
                address: format!("{:?}", info.payload), 
            }),
            Err(e) => Err(e.into()), 
        }
    }

    fn get_onchain_balance(&self) -> error::Result<u64> {
        self.sync()?;
        let balance = self.wallet.borrow().lock().unwrap().get_balance()?;
        Ok(balance.confirmed)
    }

    fn create_transaction(
        &self,
        script: ScriptBuf,
        amount: u64,
        fee_rate: u32,
    ) -> error::Result<Transaction> {
        self.sync()?;
        let wallet = self.wallet.borrow_mut();
        let wallet = wallet.lock().unwrap();
        let mut tx = wallet.build_tx();
        tx.add_recipient(ScriptBuf::from_bytes(script.into_bytes()), amount)
            .fee_rate(FeeRate::from_sat_per_kvb(fee_rate as f32))
            .enable_rbf();
        let mut psbt = tx.finish()?;
        if !wallet.sign(&mut psbt.0, SignOptions::default())? {
            error::bail!("wallet not able to sign the psbt {:?}", psbt);
        }
        if !wallet.finalize_psbt(&mut psbt.0, SignOptions::default())? {
            error::bail!("wallet impossible finalize the psbt: {:?}", psbt);
        }
        let tx: Transaction = deserialize(&serialize(&psbt.0.extract_tx()))?;
        Ok(tx)
        
    }

   
    fn list_transactions(&self) -> error::Result<Vec<Utxo>> {
        self.sync()?;
        let wallet = self.wallet.borrow();
        let wallet = wallet.lock().unwrap();
        let txs = wallet
            .list_unspent()?
            .into_iter()
            .map(|tx| Utxo {
                txid: tx.outpoint.txid.to_string(),
                vout: tx.outpoint.vout,
                reserved: tx.is_spent,
                confirmed: 0,
                amount_msat: Amount::from_btc(tx.txout.value as f64).unwrap().to_sat() * 1000_u64,
            })
            .collect::<Vec<_>>();
        Ok(txs)
    }

    
    fn sync(&self) -> error::Result<()> {
        let esplora_url = match self.network {
            Network::Bitcoin => "https://mempool.space/api",
            Network::Testnet => "https://mempool.space/testnet/api",
            _ => {
                error::bail!("network `{:?}` not supported", self.network);
            }
        };

        let wallet = self.wallet.borrow();
        let mut wallet = wallet.lock().unwrap();
        let client = bdk_esplora::esplora_client::Builder::new(esplora_url).build_blocking();
        
        let height = client.get_height()?;
    
        let external_spk = wallet.get_address(AddressIndex::New)?
            .script_pubkey();
        
        let internal_spk = wallet.get_address(AddressIndex::LastUnused)?
            .script_pubkey();
    
        let all_spks = vec![external_spk, internal_spk];
        
        log::info!("bdk start to sync");
        
        //FIXME!
        //probably feature = esplora doesn't function well
        //but using cargo build, this can download esplora and make it useful
        let scan_request = FullScanRequestBuilder::default();
        let txs = client.full_scan(scan_request, 20, 10)?;
        wallet.sync(&EsploraBlockchain::new(esplora_url, 20)?, Default::default())?;
        
        // Update the wallet with new transactions    
        log::info!(
            "bdk in sync at height {}!",
            client.get_height()?
        );
        Ok(())
    }
}

#[cfg(debug_assertions)]
impl TryFrom<(PrivateKey, Option<String>)> for BDKWalletManager {
    type Error = error::Error;

    fn try_from(value: (PrivateKey, Option<String>)) -> Result<Self, Self::Error> {
        let (wallet, keymanager) = BDKWalletManager::build_from_private_key(value.0, value.1)?;
        Ok(Self {
            wallet: RefCell::new(Mutex::new(wallet)),
            keymanager: Arc::new(keymanager),
            // This should be possible only during integration testing
            // FIXME: fix the sync method in bdk, the esplora client will crash!
            network: Network::Regtest,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lampo_common::bitcoin;
    use lampo_common::bitcoin::PrivateKey;
    use lampo_common::secp256k1::SecretKey;

    use super::{BDKWalletManager, WalletManager};

    #[test]
    fn from_private_key() {
        let pkey = PrivateKey::new(
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
            bitcoin::Network::Regtest,
        );
        let wallet = BDKWalletManager::try_from((pkey, None));
        assert!(wallet.is_ok(), "{:?}", wallet.err());
        let wallet = wallet.unwrap();
        assert!(wallet.get_onchain_address().is_ok());
    }
}
