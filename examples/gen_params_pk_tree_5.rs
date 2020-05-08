#![allow(dead_code)]

use std::fs::File;
use std::{error::Error, time::Instant};

use algebra::mnt4_753::MNT4_753;
use algebra_core::{test_rng, ToBytes};
use groth16::{generate_random_parameters, Parameters};
use rand::RngCore;

use nano_sync::circuits::mnt4::PKTree5Circuit;
use nano_sync::constants::{PK_TREE_BREADTH, PK_TREE_DEPTH, VALIDATOR_SLOTS};
use nano_sync::utils::{gen_rand_g1_mnt6, gen_rand_g2_mnt6};

fn main() -> Result<(), Box<dyn Error>> {
    // Initialize rng.
    let rng = &mut test_rng();

    // Create dummy inputs.
    let pks = vec![gen_rand_g2_mnt6(); VALIDATOR_SLOTS / PK_TREE_BREADTH];

    let pks_nodes = vec![gen_rand_g1_mnt6(); PK_TREE_DEPTH];

    let prepare_agg_pk = gen_rand_g2_mnt6();

    let commit_agg_pk = gen_rand_g2_mnt6();

    let mut pks_commitment = [0u8; 95];
    rng.fill_bytes(&mut pks_commitment);

    let mut prepare_signer_bitmap = [0u8; VALIDATOR_SLOTS / 8];
    rng.fill_bytes(&mut prepare_signer_bitmap);

    let mut prepare_agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut prepare_agg_pk_commitment);

    let mut commit_signer_bitmap = [0u8; VALIDATOR_SLOTS / 8];
    rng.fill_bytes(&mut commit_signer_bitmap);

    let mut commit_agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut commit_agg_pk_commitment);

    let position = 0;

    // Create parameters for our circuit
    println!("Starting parameter generation.");

    let start = Instant::now();

    let params: Parameters<MNT4_753> = {
        let c = PKTree5Circuit::new(
            pks,
            pks_nodes,
            prepare_agg_pk,
            commit_agg_pk,
            pks_commitment.to_vec(),
            prepare_signer_bitmap.to_vec(),
            prepare_agg_pk_commitment.to_vec(),
            commit_signer_bitmap.to_vec(),
            commit_agg_pk_commitment.to_vec(),
            position,
        );
        generate_random_parameters::<MNT4_753, _, _>(c, rng)?
    };

    println!(
        "Parameter generation finished. Took {:?} seconds",
        start.elapsed()
    );

    // Save verifying key to file.
    println!("Storing verifying key");

    let mut file = File::create("verifying_keys/pk_tree_5.bin")?;

    ToBytes::write(&params.vk.alpha_g1, &mut file)?;
    ToBytes::write(&params.vk.beta_g2, &mut file)?;
    ToBytes::write(&params.vk.gamma_g2, &mut file)?;
    ToBytes::write(&params.vk.delta_g2, &mut file)?;
    ToBytes::write(&(params.vk.gamma_abc_g1.len() as u64), &mut file)?;
    ToBytes::write(&params.vk.gamma_abc_g1, &mut file)?;

    file.sync_all()?;

    // Save proving key to file.
    println!("Storing proving key");

    let mut file = File::create("proving_keys/pk_tree_5.bin")?;

    ToBytes::write(&params.vk.alpha_g1, &mut file)?;
    ToBytes::write(&params.vk.beta_g2, &mut file)?;
    ToBytes::write(&params.vk.gamma_g2, &mut file)?;
    ToBytes::write(&params.vk.delta_g2, &mut file)?;
    ToBytes::write(&(params.vk.gamma_abc_g1.len() as u64), &mut file)?;
    ToBytes::write(&params.vk.gamma_abc_g1, &mut file)?;
    ToBytes::write(&params.beta_g1, &mut file)?;
    ToBytes::write(&params.delta_g1, &mut file)?;
    ToBytes::write(&(params.a_query.len() as u64), &mut file)?;
    ToBytes::write(&params.a_query, &mut file)?;
    ToBytes::write(&(params.b_g1_query.len() as u64), &mut file)?;
    ToBytes::write(&params.b_g1_query, &mut file)?;
    ToBytes::write(&(params.b_g2_query.len() as u64), &mut file)?;
    ToBytes::write(&params.b_g2_query, &mut file)?;
    ToBytes::write(&(params.h_query.len() as u64), &mut file)?;
    ToBytes::write(&params.h_query, &mut file)?;
    ToBytes::write(&(params.l_query.len() as u64), &mut file)?;
    ToBytes::write(&params.l_query, &mut file)?;

    file.sync_all()?;

    Ok(())
}
