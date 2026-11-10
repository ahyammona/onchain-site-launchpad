//! Upload a file (your site) into a Solana account owned by the onchain-site program.
//!
//! usage:
//!   upload <file> [--encoding brotli|raw] [--freeze]
//!
//! env:
//!   RPC_URL     default http://127.0.0.1:8899
//!   PROGRAM_ID  address of the deployed onchain-site program (required)
//!   KEYPAIR     payer/authority keypair JSON (default ~/.config/solana/id.json)

use layout::{compress, decompress, read_site, sha256, ENC_BROTLI, ENC_RAW, HEADER_LEN, MAX_ACCOUNT};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::{env, fs, str::FromStr, thread, time::Duration};

const CHUNK: usize = 900; // payload bytes per write tx (tx limit is 1232 incl. signature + overhead)
const PARALLEL: usize = 16; // concurrent write transactions

fn site_ix(program_id: Pubkey, authority: Pubkey, site: Pubkey, tag: u8, body: &[u8]) -> Instruction {
    let mut data = Vec::with_capacity(1 + body.len());
    data.push(tag);
    data.extend_from_slice(body);
    Instruction::new_with_bytes(
        program_id,
        &data,
        vec![AccountMeta::new_readonly(authority, true), AccountMeta::new(site, false)],
    )
}

fn send(rpc: &RpcClient, payer: &Keypair, extra: &[&Keypair], ixs: &[Instruction]) -> Result<(), String> {
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra);
    let bh = rpc.get_latest_blockhash().map_err(|e| e.to_string())?;
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, bh);
    rpc.send_and_confirm_transaction(&tx).map(|_| ()).map_err(|e| e.to_string())
}

fn send_retry(rpc: &RpcClient, payer: &Keypair, ixs: &[Instruction]) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 1..=5u64 {
        match send(rpc, payer, &[], ixs) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e;
                thread::sleep(Duration::from_millis(500 * attempt));
            }
        }
    }
    Err(last)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let file = args.get(1).filter(|a| !a.starts_with("--"))
        .ok_or("usage: upload <file> [--encoding brotli|raw] [--freeze]")?;
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let (enc_name, encoding) = match flag("--encoding").as_deref() {
        None | Some("brotli") => ("brotli", ENC_BROTLI),
        Some("raw") => ("raw", ENC_RAW),
        Some(_) => return Err("encoding must be brotli or raw".into()),
    };
    let freeze = args.iter().any(|a| a == "--freeze");

    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let program_id = Pubkey::from_str(&env::var("PROGRAM_ID").map_err(|_| "set PROGRAM_ID to the deployed program address")?)
        .map_err(|e| format!("bad PROGRAM_ID: {e}"))?;
    let key_path = env::var("KEYPAIR").unwrap_or_else(|_| format!("{}/.config/solana/id.json", env::var("HOME").unwrap_or_default()));
    let payer = read_keypair_file(&key_path).map_err(|e| format!("cannot read keypair {key_path}: {e}"))?;

    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let original = fs::read(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let payload = compress(&original, encoding)?;
    let hash = sha256(&payload);
    let space = HEADER_LEN + payload.len();
    if space > MAX_ACCOUNT {
        return Err(format!("site is {space} bytes; one account holds at most {MAX_ACCOUNT}"));
    }

    let rent = rpc.get_minimum_balance_for_rent_exemption(space).map_err(|e| e.to_string())?;
    let chunks = payload.len().div_ceil(CHUNK);
    println!("original {} B -> stored {} B ({enc_name}), {chunks} write txs", original.len(), payload.len());
    println!("account {space} B, rent-exempt deposit {:.6} SOL (refundable on close)", rent as f64 / 1e9);

    // 1. create + init in one transaction
    let site = Keypair::new();
    let mut init_body = Vec::with_capacity(37);
    init_body.push(encoding);
    init_body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    init_body.extend_from_slice(&hash);
    send(
        &rpc,
        &payer,
        &[&site],
        &[
            system_instruction::create_account(&payer.pubkey(), &site.pubkey(), rent, space as u64, &program_id),
            site_ix(program_id, payer.pubkey(), site.pubkey(), 0, &init_body),
        ],
    )?;
    println!("created site account {}", site.pubkey());

    // 2. write chunks in parallel batches, retrying individual failures
    let mut done = 0;
    for batch_start in (0..chunks).step_by(PARALLEL) {
        let batch_end = (batch_start + PARALLEL).min(chunks);
        let results: Vec<Result<(), String>> = thread::scope(|s| {
            let handles: Vec<_> = (batch_start..batch_end)
                .map(|i| {
                    let (rpc, payer, payload) = (&rpc, &payer, &payload);
                    let site_pk = site.pubkey();
                    s.spawn(move || {
                        let offset = i * CHUNK;
                        let end = (offset + CHUNK).min(payload.len());
                        let mut body = (offset as u32).to_le_bytes().to_vec();
                        body.extend_from_slice(&payload[offset..end]);
                        send_retry(
                            rpc,
                            payer,
                            &[
                                ComputeBudgetInstruction::set_compute_unit_limit(5_000),
                                site_ix(program_id, payer.pubkey(), site_pk, 1, &body),
                            ],
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("thread panicked".into()))).collect()
        });
        for r in results {
            r?;
        }
        done += batch_end - batch_start;
        print!("\rwritten {done}/{chunks}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!();

    // 3. read back and verify before declaring success (or freezing)
    let acct = rpc.get_account_data(&site.pubkey()).map_err(|e| e.to_string())?;
    let stored = read_site(&acct)?;
    if !stored.hash_ok {
        return Err("hash mismatch after upload; do not freeze. Re-run or close the account.".into());
    }
    if decompress(stored.payload, encoding)? != original {
        return Err("round-trip mismatch".into());
    }
    println!("verified: onchain bytes hash-match and decompress to the original");

    if freeze {
        send(&rpc, &payer, &[], &[site_ix(program_id, payer.pubkey(), site.pubkey(), 3, &[])])?;
        println!("frozen: this site can no longer be changed or closed");
    }

    println!("\nsite address: {}", site.pubkey());
    println!("open via gateway: http://localhost:8080/{}", site.pubkey());
    Ok(())
}
