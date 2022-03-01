use anchor_client::{
    solana_sdk::{
        instruction::Instruction,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        system_instruction, system_program, sysvar,
    },
    Client,
};
use anchor_lang::prelude::AccountMeta;
use anyhow::Result;
use rand::rngs::OsRng;
use spl_associated_token_account::{create_associated_token_account, get_associated_token_address};
use spl_token::{
    instruction::{initialize_mint, mint_to},
    ID as TOKEN_PROGRAM_ID,
};
use std::{str::FromStr, sync::Arc};

use mpl_candy_machine::accounts as nft_accounts;
use mpl_candy_machine::instruction as nft_instruction;
use mpl_candy_machine::{CandyMachine, WhitelistMintMode, ID as CANDY_MACHINE_PROGRAM_ID};

use crate::cache::load_cache;
use crate::candy_machine::*;
use crate::common::*;
use crate::mint::pdas::*;

pub struct MintArgs {
    pub keypair: Option<String>,
    pub rpc_url: Option<String>,
    pub cache: String,
    pub number: Option<u64>,
}

pub fn process_mint(args: MintArgs) -> Result<()> {
    let sugar_config = sugar_setup(args.keypair, args.rpc_url)?;
    let cache = load_cache(&args.cache)?;
    let client = Arc::new(setup_client(&sugar_config)?);

    let candy_machine_id = match Pubkey::from_str(&cache.program.candy_machine) {
        Ok(candy_machine_id) => candy_machine_id,
        Err(_) => {
            let error = anyhow!(
                "Failed to parse candy machine id: {}",
                cache.program.candy_machine
            );
            error!("{:?}", error);
            return Err(error);
        }
    };

    let candy_machine_state = Arc::new(get_candy_machine_state(&sugar_config, &candy_machine_id)?);

    let number = match args.number {
        Some(number) => number,
        None => 1,
    };

    info!("Minting NFT from candy machine: {}", &candy_machine_id);
    info!("Candy machine program id: {:?}", CANDY_MACHINE_PROGRAM_ID);

    if number == 1 {
        mint(
            Arc::clone(&client),
            candy_machine_id,
            Arc::clone(&candy_machine_state),
        )?;
    } else {
        for _n in 1..=number {
            mint(
                Arc::clone(&client),
                candy_machine_id,
                Arc::clone(&candy_machine_state),
            )?;
        }
    }

    Ok(())
}

pub fn mint(
    client: Arc<Client>,
    candy_machine_id: Pubkey,
    candy_machine_state: Arc<CandyMachine>,
) -> Result<()> {
    let pid = "cndy3Z4yapfJBmL3ShUp5exZKqR3z33thTzeNMm2gRZ"
        .parse()
        .expect("Failed to parse PID");

    let program = client.program(pid);
    let payer = program.payer();
    let wallet = candy_machine_state.wallet;

    let candy_machine_data = &candy_machine_state.data;

    let nft_mint = Keypair::new();
    let metaplex_program_id = Pubkey::from_str(METAPLEX_PROGRAM_ID)?;

    // Allocate memory for the account
    let min_rent = program
        .rpc()
        .get_minimum_balance_for_rent_exemption(MINT_LAYOUT as usize)?;

    // Create mint account
    let create_mint_account_ix = system_instruction::create_account(
        &payer,
        &nft_mint.pubkey(),
        min_rent,
        MINT_LAYOUT,
        &TOKEN_PROGRAM_ID,
    );

    // Initalize mint ix
    let init_mint_ix = initialize_mint(
        &TOKEN_PROGRAM_ID,
        &nft_mint.pubkey(),
        &payer,
        Some(&payer),
        0,
    )?;

    // Derive associated token account
    let assoc = get_associated_token_address(&payer, &nft_mint.pubkey());

    // Create associated account instruction
    let create_assoc_account_ix =
        create_associated_token_account(&payer, &payer, &nft_mint.pubkey());

    // Mint to instruction
    let mint_to_ix = mint_to(
        &TOKEN_PROGRAM_ID,
        &nft_mint.pubkey(),
        &assoc,
        &payer,
        &[],
        1,
    )?;

    let mut additional_instructions: Vec<Instruction> = Vec::new();
    let mut cleanup_instructions: Vec<Instruction> = Vec::new();
    let mut additional_accounts: Vec<AccountMeta> = Vec::new();
    let mut additional_signers: Vec<Keypair> = Vec::new();

    // Check whitelist mint settings
    if let Some(wl_mint_settings) = &candy_machine_data.whitelist_mint_settings {
        let whitelist_token = get_ata_for_mint(&wl_mint_settings.mint, &payer);

        additional_accounts.push(AccountMeta {
            pubkey: whitelist_token,
            is_signer: false,
            is_writable: true,
        });

        if wl_mint_settings.mode == WhitelistMintMode::BurnEveryTime {
            let whitelist_burn_authority = Keypair::generate(&mut OsRng);

            additional_accounts.push(AccountMeta {
                pubkey: wl_mint_settings.mint,
                is_signer: false,
                is_writable: true,
            });
            additional_accounts.push(AccountMeta {
                pubkey: whitelist_burn_authority.pubkey().clone(),
                is_signer: true,
                is_writable: false,
            });

            let ata_exists = !program.rpc().get_account_data(&whitelist_token)?.is_empty();

            if ata_exists {
                let approve_ix = spl_token::instruction::approve(
                    &TOKEN_PROGRAM_ID,
                    &whitelist_token,
                    &whitelist_burn_authority.pubkey(),
                    &payer,
                    &[],
                    1,
                )?;
                let revoke_ix = spl_token::instruction::revoke(
                    &TOKEN_PROGRAM_ID,
                    &whitelist_token,
                    &payer,
                    &[],
                )?;

                additional_instructions.push(approve_ix);
                cleanup_instructions.push(revoke_ix);
            }

            additional_signers.push(whitelist_burn_authority);
        }
    }

    if let Some(token_mint) = candy_machine_state.token_mint {
        let transfer_authority = Keypair::generate(&mut OsRng);

        let user_paying_account_address = get_ata_for_mint(&token_mint, &payer);

        additional_accounts.push(AccountMeta {
            pubkey: user_paying_account_address.clone(),
            is_signer: false,
            is_writable: true,
        });

        additional_accounts.push(AccountMeta {
            pubkey: transfer_authority.pubkey(),
            is_signer: true,
            is_writable: false,
        });

        let ata_exists = !program.rpc().get_account_data(&token_mint)?.is_empty();

        if ata_exists {
            let approve_ix = spl_token::instruction::approve(
                &TOKEN_PROGRAM_ID,
                &user_paying_account_address,
                &transfer_authority.pubkey(),
                &payer,
                &[],
                candy_machine_data.price,
            )?;
            let revoke_ix = spl_token::instruction::revoke(
                &TOKEN_PROGRAM_ID,
                &user_paying_account_address,
                &payer,
                &[],
            )?;

            additional_instructions.push(approve_ix);
            cleanup_instructions.push(revoke_ix);
        }

        additional_signers.push(transfer_authority);
    }

    let metadata_pda = get_metadata_pda(&nft_mint.pubkey());
    let master_edition_pda = get_master_edition_pda(&nft_mint.pubkey());
    let (candy_machine_creator_pda, creator_bump) =
        get_candy_machine_creator_pda(&candy_machine_id);

    let mut builder = program
        .request()
        .instruction(create_mint_account_ix)
        .instruction(init_mint_ix)
        .instruction(create_assoc_account_ix)
        .instruction(mint_to_ix)
        .signer(&nft_mint)
        .accounts(nft_accounts::MintNFT {
            candy_machine: candy_machine_id,
            candy_machine_creator: candy_machine_creator_pda,
            payer,
            wallet,
            metadata: metadata_pda,
            mint: nft_mint.pubkey(),
            mint_authority: payer,
            update_authority: payer,
            master_edition: master_edition_pda,
            token_metadata_program: metaplex_program_id,
            token_program: TOKEN_PROGRAM_ID,
            system_program: system_program::id(),
            rent: sysvar::rent::ID,
            clock: sysvar::clock::ID,
            recent_blockhashes: sysvar::recent_blockhashes::ID,
            instruction_sysvar_account: sysvar::instructions::ID,
        })
        .args(nft_instruction::MintNft { creator_bump });

    // Add additional instructions based on candy machine settings.
    if !additional_instructions.is_empty() {
        for instruction in additional_instructions {
            builder = builder.instruction(instruction);
        }
    }

    if !additional_accounts.is_empty() {
        for account in additional_accounts {
            builder = builder.accounts(account);
        }
    }

    if !additional_signers.is_empty() {
        for signer in &additional_signers {
            builder = builder.signer(signer);
        }
    }

    let sig = builder.send()?;

    // Cleanup instructions, such as revoke token burn authority, require a separate transaction.
    let mut builder = program.request();

    if !cleanup_instructions.is_empty() {
        for instruction in cleanup_instructions {
            builder = builder.instruction(instruction);
        }
    }

    let sig2 = builder.send()?;

    info!("Minted! TxId: {}", sig);
    info!("Cleanup TxId: {}", sig2);
    println!("Minted! TxId: {}", sig);
    Ok(())
}
