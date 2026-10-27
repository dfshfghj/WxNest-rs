use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;

use wechat_core::account::{decrypt_account_with_options, AccountDecryptOptions};
use wechat_core::crypto::isaac64::{Isaac64, WordFormat};
use wechat_core::image_key::resolve_local_image_key;
use wechat_core::media::{decrypt_media_bytes, MediaKeys};
use wechat_core::sqlcipher::{
    decrypt_database, scan_account_databases_from_path, validate_realtime_database_key,
};

#[derive(Debug, Parser)]
#[command(name = "wechat-core", version, about = "WeChat 4.x decryption core")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Decrypt one SQLCipher/WCDB 4.x database.")]
    DecryptDb {
        db: PathBuf,
        out: PathBuf,
        #[arg(long)]
        key_hex: String,
    },

    #[command(about = "Decrypt every database of one account into a flat account directory.")]
    DecryptAccount {
        #[arg(help = "Account directory, its db_storage directory, or the parent data root.")]
        input: PathBuf,

        #[arg(long, help = "Directory that receives <account>/<database files>.")]
        out: PathBuf,
        #[arg(long)]
        key_hex: String,

        #[arg(
            long,
            help = "Keep existing decrypted files instead of overwriting them."
        )]
        skip_existing: bool,
    },

    #[command(about = "Decrypt one WeChat DAT/media file.")]
    DecryptMedia {
        input: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        version: Option<i32>,
        #[arg(long)]
        xor_key: Option<String>,
        #[arg(long)]
        aes_key: Option<String>,
        #[arg(long)]
        aes_key_hex: Option<String>,
    },

    #[command(about = "Resolve a WeChat image key from kvcomm codes and local V2 templates.")]
    ResolveImageKey {
        #[arg(long)]
        kvcomm_dir: PathBuf,
        #[arg(long)]
        account_dir: PathBuf,
        #[arg(long)]
        target_wxid: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long = "native-wxid")]
        native_wxids: Vec<String>,
        #[arg(long, default_value_t = 32)]
        template_limit: usize,
        #[arg(long, default_value_t = 500)]
        max_fallback_dirs: usize,
    },

    #[command(about = "Validate a key against the message and session database roles.")]
    ValidateKey {
        #[arg(long)]
        db_storage: PathBuf,
        #[arg(long)]
        key_hex: String,
    },

    #[command(about = "Scan a db_storage directory for account databases.")]
    ScanAccount { db_storage: PathBuf },

    #[command(about = "Generate an ISAAC-64 keystream.")]
    Isaac64 {
        #[arg(long, default_value = "")]
        seed: String,
        #[arg(long, default_value_t = 64)]
        size: usize,
        #[arg(long, default_value = "be_swap32")]
        format: String,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(success) => {
            if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::DecryptDb { db, out, key_hex } => {
            let result = decrypt_database(&db, &out, &key_hex);
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(result.success)
        }
        Command::DecryptAccount {
            input,
            out,
            key_hex,
            skip_existing,
        } => {
            let report = decrypt_account_with_options(
                &input,
                &out,
                &key_hex,
                &AccountDecryptOptions { skip_existing },
            );
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(report.failed_count == 0 && report.status != "error")
        }
        Command::DecryptMedia {
            input,
            out,
            version,
            xor_key,
            aes_key,
            aes_key_hex,
        } => {
            let data = std::fs::read(&input)?;
            let keys = MediaKeys {
                xor_key: xor_key.as_deref().map(parse_u8).transpose()?,
                aes_key: match (aes_key, aes_key_hex) {
                    (Some(value), _) => Some(value.into_bytes()),
                    (_, Some(value)) => Some(hex::decode(value)?),
                    _ => None,
                },
            };
            let result = decrypt_media_bytes(&data, &keys, version, None);
            if let Some(out) = out.as_ref() {
                std::fs::write(out, &result.data)?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "media_type": result.media_type,
                    "size": result.data.len(),
                    "dat_version": result.dat_version,
                    "used_magic_fallback": result.used_magic_fallback,
                    "output": out.as_ref().map(|path| path.display().to_string()),
                }))?
            );
            Ok(true)
        }
        Command::ResolveImageKey {
            kvcomm_dir,
            account_dir,
            target_wxid,
            account,
            native_wxids,
            template_limit,
            max_fallback_dirs,
        } => {
            let resolution = resolve_local_image_key(
                &kvcomm_dir,
                &account_dir,
                target_wxid.as_deref(),
                account.as_deref(),
                &native_wxids,
                None,
                template_limit,
                max_fallback_dirs,
            );
            match resolution {
                Some(value) => {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    Ok(true)
                }
                None => {
                    println!("null");
                    Ok(false)
                }
            }
        }
        Command::ValidateKey {
            db_storage,
            key_hex,
        } => {
            let result = validate_realtime_database_key(&db_storage, &key_hex);
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(result.valid)
        }
        Command::ScanAccount { db_storage } => {
            let result = scan_account_databases_from_path(&db_storage);
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(result.status == "success")
        }
        Command::Isaac64 { seed, size, format } => {
            let format = format.parse::<WordFormat>()?;
            let mut isaac = Isaac64::new(&seed);
            let keystream = isaac.generate_keystream(size, format);
            println!("{}", hex::encode(keystream));
            Ok(true)
        }
    }
}

fn parse_u8(value: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let value = value.trim();
    if let Some(rest) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Ok(u8::from_str_radix(rest, 16)?)
    } else {
        Ok(value.parse::<u8>()?)
    }
}
