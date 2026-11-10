//! Minimal "blob store" program for hosting a website in an account.
//!
//! Flow (client side):
//!   1. System program creates the account with owner = this program,
//!      space = HEADER_LEN + payload length, rent-exempt lamports.
//!   2. Init   (tag 0): sets header (encoding, length, sha256, authority).
//!   3. Write  (tag 1): writes `data` at `offset` in the payload. Repeat.
//!   4. Freeze (tag 3): optional, makes the site immutable forever.
//!   5. Close  (tag 2): authority reclaims rent, site disappears.
//!
//! Accounts for every instruction: [0] authority (signer), [1] site account (writable).

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    pubkey::Pubkey,
    system_program,
};

entrypoint!(process);

const MAGIC: [u8; 4] = *b"SOLW";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 74;

// Header offsets
const OFF_VERSION: usize = 4;
const OFF_ENC: usize = 5;
const OFF_AUTH: usize = 6;
const OFF_LEN: usize = 38;
const OFF_HASH: usize = 42;

fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    let it = &mut accounts.iter();
    let authority = next_account_info(it)?;
    let site = next_account_info(it)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if site.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if !site.is_writable {
        return Err(ProgramError::InvalidAccountData);
    }

    match tag {
        0 => init(authority, site, rest),
        1 => write(authority, site, rest),
        2 => close(authority, site),
        3 => freeze(authority, site),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// rest = encoding(u8) | length(u32 LE) | sha256(32)
fn init(authority: &AccountInfo, site: &AccountInfo, rest: &[u8]) -> ProgramResult {
    if rest.len() != 1 + 4 + 32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let encoding = rest[0];
    let length = u32::from_le_bytes(rest[1..5].try_into().unwrap()) as usize;
    let hash = &rest[5..37];

    let mut d = site.try_borrow_mut_data()?;
    if d.len() != HEADER_LEN + length {
        msg!("account size must be HEADER_LEN + length");
        return Err(ProgramError::InvalidAccountData);
    }
    if d[0..4] == MAGIC {
        msg!("already initialized");
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    d[0..4].copy_from_slice(&MAGIC);
    d[OFF_VERSION] = VERSION;
    d[OFF_ENC] = encoding;
    d[OFF_AUTH..OFF_AUTH + 32].copy_from_slice(authority.key.as_ref());
    d[OFF_LEN..OFF_LEN + 4].copy_from_slice(&(length as u32).to_le_bytes());
    d[OFF_HASH..OFF_HASH + 32].copy_from_slice(hash);
    Ok(())
}

fn check_authority(d: &[u8], authority: &AccountInfo) -> ProgramResult {
    if d.len() < HEADER_LEN || d[0..4] != MAGIC {
        return Err(ProgramError::UninitializedAccount);
    }
    if d[OFF_AUTH..OFF_AUTH + 32] != *authority.key.as_ref() {
        msg!("wrong authority, or site is frozen");
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

/// rest = offset(u32 LE) | bytes...
fn write(authority: &AccountInfo, site: &AccountInfo, rest: &[u8]) -> ProgramResult {
    if rest.len() < 4 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let offset = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
    let bytes = &rest[4..];

    let mut d = site.try_borrow_mut_data()?;
    check_authority(&d, authority)?;
    let length = u32::from_le_bytes(d[OFF_LEN..OFF_LEN + 4].try_into().unwrap()) as usize;
    let end = offset
        .checked_add(bytes.len())
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if end > length {
        msg!("write past end of payload");
        return Err(ProgramError::InvalidInstructionData);
    }
    d[HEADER_LEN + offset..HEADER_LEN + end].copy_from_slice(bytes);
    Ok(())
}

/// Zero the authority: nobody can write or close afterwards.
fn freeze(authority: &AccountInfo, site: &AccountInfo) -> ProgramResult {
    let mut d = site.try_borrow_mut_data()?;
    check_authority(&d, authority)?;
    d[OFF_AUTH..OFF_AUTH + 32].fill(0);
    Ok(())
}

fn close(authority: &AccountInfo, site: &AccountInfo) -> ProgramResult {
    {
        let d = site.try_borrow_data()?;
        check_authority(&d, authority)?;
    }
    let lamports = site.lamports();
    **authority.try_borrow_mut_lamports()? = authority
        .lamports()
        .checked_add(lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    **site.try_borrow_mut_lamports()? = 0;
    site.assign(&system_program::ID);
    site.resize(0)?;
    Ok(())
}
