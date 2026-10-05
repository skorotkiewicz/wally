mod wallet;

use anyhow::{Context, Result, ensure};
use bitcoin::{OutPoint, Transaction, Txid, consensus};
use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};
use std::{fs, io::{self, Read, Write}, path::{Path, PathBuf}, time::Duration};
use wallet::{Coin, Utxo, Wallet, display_amount, parse_amount};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(version, about = "Standalone Bitcoin and Dogecoin wallets (mainnet)")]
struct Cli {
    /// Wallet storage directory (default: ~/.wally).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a password-encrypted wallet, offline.
    Init {
        #[arg(long, value_enum)]
        coin: Coin,
        /// Defaults to the coin name; use a different name for additional wallets.
        #[arg(long)]
        name: Option<String>,
    },
    /// List local wallets and their receiving addresses, offline.
    Ls,
    /// Show the wallet's receiving address, offline.
    #[command(alias = "recive")]
    Receive { wallet: String },
    /// Query the public API for this wallet's balance.
    Balance { wallet: String },
    /// Sign locally and broadcast after interactive confirmation.
    Send {
        wallet: String,
        #[arg(long)]
        to: String,
        /// Coin units, not satoshis; at most eight decimal places.
        #[arg(long)]
        amount: String,
    },
}

fn wallet_path(dir: &Path, name: &str) -> Result<PathBuf> {
    ensure!(!name.is_empty() && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Wallet names must be 1-64 ASCII letters, digits, '-' or '_'");
    Ok(dir.join(format!("{name}.json")))
}

fn load(dir: &Path, name: &str) -> Result<Wallet> {
    let path = wallet_path(dir, name)?;
    let file = fs::File::open(&path).with_context(|| format!("Cannot open wallet {name}"))?;
    let wallet: Wallet = serde_json::from_reader(file.take(16_384))
        .with_context(|| format!("Invalid wallet file: {}", path.display()))?;
    wallet.validate()?;
    Ok(wallet)
}

fn save(dir: &Path, name: &str, wallet: &Wallet) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)] {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(wallet_path(dir, name)?)
        .context("Cannot create wallet file; existing wallets are never overwritten")?;
    file.write_all(&serde_json::to_vec_pretty(wallet)?)?;
    file.sync_all()?;
    Ok(())
}

struct Api {
    client: Client,
    base: String,
    token: Option<String>,
}

impl Api {
    fn new(coin: Coin) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(30))
                .https_only(true).redirect(reqwest::redirect::Policy::none()).build()?,
            base: format!("https://api.blockcypher.com/v1/{}", coin.chain()),
            token: std::env::var("WALLY_API_TOKEN").ok(),
        })
    }

    fn response(&self, mut request: RequestBuilder) -> Result<Value> {
        if let Some(token) = &self.token { request = request.query(&[("token", token)]); }
        let response = request.send().context("Cannot reach BlockCypher")?
            .error_for_status().context("BlockCypher rejected the request (possibly rate-limited)")?;
        Ok(serde_json::from_reader(response.take(2 * 1024 * 1024))?)
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.response(self.client.get(format!("{}{path}", self.base)))
    }
}

fn number(value: &Value, field: &str) -> Result<u64> {
    value[field].as_u64().with_context(|| format!("Invalid API field: {field}"))
}

fn verify_previous(raw: &str, utxo: &Utxo, own: &bitcoin::ScriptBuf) -> Result<()> {
    let previous: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
    ensure!(previous.compute_txid() == utxo.outpoint.txid, "Previous transaction hash mismatch");
    let output = previous.output.get(utxo.outpoint.vout as usize)
        .context("Previous output does not exist")?;
    ensure!(output.value.to_sat() == utxo.value && output.script_pubkey == *own,
        "Provider returned an incorrect amount or a foreign UTXO");
    Ok(())
}

fn send(wallet: &Wallet, to: &str, amount: &str) -> Result<()> {
    let amount = parse_amount(amount)?;
    let destination = wallet.coin.script(to).context("Invalid recipient address for this coin")?;
    ensure!(amount >= wallet.coin.dust(), "Amount is below the dust limit");
    let own = wallet.coin.script(&wallet.address)?;
    let api = Api::new(wallet.coin)?;
    let rate = number(&api.get("")?, "high_fee_per_kb")?;
    let rate = if wallet.coin == Coin::Dogecoin { rate.max(1_000_000) } else { rate.max(1000) };
    let address = api.get(&format!("/addrs/{}?unspentOnly=true&limit=2000", wallet.address))?;
    ensure!(address["hasMore"].as_bool() != Some(true),
        "Too many transactions for this simple wallet; sending requires a complete UTXO response");
    ensure!(number(&address, "unconfirmed_n_tx")? == 0,
        "Wait for pending transactions to confirm before sending");
    let mut utxos = Vec::new();
    if let Some(refs) = address["txrefs"].as_array() {
        for item in refs {
            if item["spent"].as_bool() != Some(false)
                || item["double_spend"].as_bool() == Some(true)
                || number(item, "confirmations")? == 0 { continue; }
            let index = item["tx_output_n"].as_i64().context("Invalid output index")?;
            if index < 0 { continue; }
            let txid: Txid = item["tx_hash"].as_str().context("Missing transaction hash")?.parse()?;
            utxos.push(Utxo {
                outpoint: OutPoint { txid, vout: u32::try_from(index)? },
                value: number(item, "value")?,
            });
        }
    }
    let (mut tx, fee) = wallet::plan(&utxos, &own, &destination, amount, rate, wallet.coin.dust())?;
    // Verify amounts against hash-checked raw transactions, not just provider balances.
    for (input, utxo) in tx.input.iter().zip(&utxos) {
        let previous = api.get(&format!("/txs/{}?includeHex=true", input.previous_output.txid))?;
        verify_previous(previous["hex"].as_str().context("Provider omitted raw transaction")?, utxo, &own)?;
    }
    println!("Send {} {} to {to}", display_amount(amount), wallet.coin.name());
    println!("Network fee: {} {}", display_amount(fee), wallet.coin.name());
    print!("Type 'send' to confirm: ");
    io::stdout().flush()?;
    let mut confirmation = String::new();
    io::stdin().read_line(&mut confirmation)?;
    if confirmation.trim() != "send" { println!("Cancelled."); return Ok(()); }
    let password = Zeroizing::new(rpassword::prompt_password("Wallet password: ")?);
    let key = wallet.unlock(&password)?;
    wallet::sign(&mut tx, &own, &key)?;
    let txid = tx.compute_txid();
    println!("Transaction ID: {txid}");
    io::stdout().flush()?;
    api.response(api.client.post(format!("{}/txs/push", api.base))
        .json(&json!({ "tx": hex::encode(consensus::serialize(&tx)) })))
        .with_context(|| format!("Broadcast status unknown. Check {txid} before retrying; do not send again blindly"))?;
    println!("Broadcast accepted; awaiting network confirmation.");
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let dir = match cli.data_dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            .context("Set --data-dir because no home directory was found")?).join(".wally"),
    };
    match cli.command {
        Command::Init { coin, name } => {
            let name = name.unwrap_or_else(|| coin.name().to_owned());
            ensure!(!wallet_path(&dir, &name)?.try_exists()?, "Wallet {name} already exists");
            let password = Zeroizing::new(rpassword::prompt_password("New wallet password (12+ characters): ")?);
            let confirmation = Zeroizing::new(rpassword::prompt_password("Repeat password: ")?);
            ensure!(*password == *confirmation, "Passwords do not match");
            let wallet = Wallet::create(coin, &password)?;
            save(&dir, &name, &wallet)?;
            println!("Created {name} ({})\nReceive: {}", coin.name(), wallet.address);
            println!("Back up {} and keep your password. There is no password recovery.",
                wallet_path(&dir, &name)?.display());
        }
        Command::Ls => {
            if !dir.try_exists()? { println!("No wallets. Use wally init --coin bitcoin or dogecoin."); return Ok(()); }
            let mut names = Vec::new();
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json") {
                    names.push(path.file_stem().and_then(|s| s.to_str())
                        .context("Invalid wallet filename")?.to_owned());
                }
            }
            names.sort();
            if names.is_empty() { println!("No wallets."); }
            for name in names {
                let wallet = load(&dir, &name)?;
                println!("{name}\t{}\t{}", wallet.coin.name(), wallet.address);
            }
        }
        Command::Receive { wallet } => println!("{}", load(&dir, &wallet)?.address),
        Command::Balance { wallet } => {
            let wallet = load(&dir, &wallet)?;
            let response = Api::new(wallet.coin)?.get(&format!("/addrs/{}/balance", wallet.address))?;
            println!("{} {} (including pending transactions)",
                display_amount(number(&response, "final_balance")?), wallet.coin.name());
        }
        Command::Send { wallet, to, amount } => send(&load(&dir, &wallet)?, &to, &amount)?,
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Amount, ScriptBuf, TxOut, absolute, transaction};

    #[test]
    fn boundaries_and_previous_output_check() -> Result<()> {
        for name in ["", "../bitcoin", "a/b", "a.b", "a\\b"] {
            assert!(wallet_path(Path::new("wallets"), name).is_err());
        }
        assert_eq!(wallet_path(Path::new("wallets"), "my-btc_1")?, Path::new("wallets/my-btc_1.json"));
        let own = ScriptBuf::new();
        let previous = Transaction {
            version: transaction::Version::ONE, lock_time: absolute::LockTime::ZERO,
            input: vec![], output: vec![TxOut { value: Amount::from_sat(123), script_pubkey: own.clone() }],
        };
        let raw = hex::encode(consensus::serialize(&previous));
        let mut utxo = Utxo { outpoint: OutPoint { txid: previous.compute_txid(), vout: 0 }, value: 123 };
        verify_previous(&raw, &utxo, &own)?;
        utxo.value = 124;
        assert!(verify_previous(&raw, &utxo, &own).is_err());
        utxo.value = 123;
        utxo.outpoint.vout = 1;
        assert!(verify_previous(&raw, &utxo, &own).is_err());
        Ok(())
    }
}
