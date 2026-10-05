mod backend;
mod chain;
mod peer;
mod wallet;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use wallet::{Coin, Wallet, display_amount, parse_amount};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(version, about = "Standalone Bitcoin and Dogecoin wallets (mainnet)")]
struct Cli {
    /// Wallet storage directory (default: ~/.wally).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Optional direct peers (HOST:PORT); repeat for at least two different IPs.
    #[arg(long, global = true)]
    peer: Vec<String>,
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
    /// Synchronize with peers and show the confirmed balance.
    Balance { wallet: String },
    /// Synchronize headers and scan blocks for this wallet.
    Sync { wallet: String },
    /// Relay the exact saved pending transaction, never a second payment.
    Rebroadcast { wallet: String },
    /// Sign locally and broadcast after interactive confirmation.
    Send {
        wallet: String,
        #[arg(long)]
        to: String,
        /// Coin units, not satoshis; at most eight decimal places.
        #[arg(long)]
        amount: String,
        /// Network fee in coin units per 1000 bytes (defaults: 0.0001 BTC / 0.01 DOGE).
        #[arg(long)]
        fee_per_kb: Option<String>,
    },
}

fn wallet_path(dir: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Wallet names must be 1-64 ASCII letters, digits, '-' or '_'"
    );
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(wallet_path(dir, name)?)
        .context("Cannot create wallet file; existing wallets are never overwritten")?;
    file.write_all(&serde_json::to_vec_pretty(wallet)?)?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn send(
    dir: &Path,
    name: &str,
    wallet: &Wallet,
    peers: &[String],
    to: &str,
    amount: &str,
    fee_per_kb: Option<&str>,
) -> Result<()> {
    let amount = parse_amount(amount)?;
    let destination = wallet
        .coin
        .script(to)
        .context("Invalid recipient address for this coin")?;
    ensure!(
        amount >= wallet.coin.dust(),
        "Amount is below the dust limit"
    );
    let rate = fee_per_kb
        .map(parse_amount)
        .transpose()?
        .unwrap_or(wallet.coin.default_fee_per_kb());
    ensure!(
        rate >= if wallet.coin == Coin::Dogecoin {
            1_000_000
        } else {
            1000
        },
        "Fee rate is below the supported relay minimum"
    );
    let own = wallet.coin.script(&wallet.address)?;
    let mut session = backend::Session::sync(dir, name, wallet, peers)?;
    let (mut tx, fee) = wallet::plan(
        &session.utxos()?,
        &own,
        &destination,
        amount,
        rate,
        wallet.coin.dust(),
    )?;
    println!(
        "Send {} {} to {to}",
        display_amount(amount),
        wallet.coin.name()
    );
    println!(
        "Network fee: {} {} (fixed rate, not a confirmation-time estimate)",
        display_amount(fee),
        wallet.coin.name()
    );
    print!("Type 'send' to confirm: ");
    io::stdout().flush()?;
    let mut confirmation = String::new();
    io::stdin().read_line(&mut confirmation)?;
    if confirmation.trim() != "send" {
        println!("Cancelled.");
        return Ok(());
    }
    let password = Zeroizing::new(rpassword::prompt_password("Wallet password: ")?);
    let key = wallet.unlock(&password)?;
    wallet::sign(&mut tx, &own, &key)?;
    println!("Transaction ID: {}", tx.compute_txid());
    io::stdout().flush()?;
    session.broadcast(&tx)
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let dir = match cli.data_dir {
        Some(dir) => dir,
        None => PathBuf::from(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .context("Set --data-dir because no home directory was found")?,
        )
        .join(".wally"),
    };
    match cli.command {
        Command::Init { coin, name } => {
            let name = name.unwrap_or_else(|| coin.name().to_owned());
            ensure!(
                !wallet_path(&dir, &name)?.try_exists()?,
                "Wallet {name} already exists"
            );
            let password = Zeroizing::new(rpassword::prompt_password(
                "New wallet password (12+ characters): ",
            )?);
            let confirmation = Zeroizing::new(rpassword::prompt_password("Repeat password: ")?);
            ensure!(*password == *confirmation, "Passwords do not match");
            let wallet = Wallet::create(coin, &password)?;
            save(&dir, &name, &wallet)?;
            println!(
                "Created {name} ({})\nReceive: {}",
                coin.name(),
                wallet.address
            );
            println!(
                "Back up {} and keep your password. There is no password recovery.",
                wallet_path(&dir, &name)?.display()
            );
        }
        Command::Ls => {
            if !dir.try_exists()? {
                println!("No wallets. Use wally init --coin bitcoin or dogecoin.");
                return Ok(());
            }
            let mut names = Vec::new();
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json") {
                    names.push(
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .context("Invalid wallet filename")?
                            .to_owned(),
                    );
                }
            }
            names.sort();
            if names.is_empty() {
                println!("No wallets.");
            }
            for name in names {
                let wallet = load(&dir, &name)?;
                println!("{name}\t{}\t{}", wallet.coin.name(), wallet.address);
            }
        }
        Command::Receive { wallet } => println!("{}", load(&dir, &wallet)?.address),
        Command::Balance { wallet: name } => {
            let wallet = load(&dir, &name)?;
            let session = backend::Session::sync(&dir, &name, &wallet, &cli.peer)?;
            println!(
                "{} {} confirmed (may include immature coinbase outputs)",
                display_amount(session.balance()?),
                wallet.coin.name()
            );
            if session.has_pending()? {
                println!("A saved outgoing payment is still pending.");
            }
        }
        Command::Sync { wallet: name } => {
            let wallet = load(&dir, &name)?;
            backend::Session::sync(&dir, &name, &wallet, &cli.peer)?;
            println!("Wallet synchronized.");
        }
        Command::Rebroadcast { wallet: name } => {
            let wallet = load(&dir, &name)?;
            backend::Session::sync(&dir, &name, &wallet, &cli.peer)?.rebroadcast()?;
        }
        Command::Send {
            wallet: name,
            to,
            amount,
            fee_per_kb,
        } => send(
            &dir,
            &name,
            &load(&dir, &name)?,
            &cli.peer,
            &to,
            &amount,
            fee_per_kb.as_deref(),
        )?,
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
    #[test]
    fn wallet_name_boundary() -> Result<()> {
        for name in ["", "../bitcoin", "a/b", "a.b", "a\\b"] {
            assert!(wallet_path(Path::new("wallets"), name).is_err());
        }
        assert_eq!(
            wallet_path(Path::new("wallets"), "my-btc_1")?,
            Path::new("wallets/my-btc_1.json")
        );
        Ok(())
    }
}
