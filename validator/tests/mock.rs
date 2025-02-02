use futures::{future, StreamExt};
use tokio::time;

use nimiq_block::{MultiSignature, SignedViewChange, ViewChange};
use nimiq_blockchain::{AbstractBlockchain, BlockchainEvent};
use nimiq_bls::{AggregateSignature, KeyPair as BlsKeyPair};
use nimiq_build_tools::genesis::GenesisBuilder;
use nimiq_collections::BitSet;
use nimiq_database::volatile::VolatileEnvironment;
use nimiq_handel::update::{LevelUpdate, LevelUpdateMessage};
use nimiq_keys::{Address, KeyPair, SecureGenerate};
use nimiq_network_interface::network::Network;
use nimiq_network_mock::{MockHub, MockNetwork};
use nimiq_test_utils::validator::{
    build_validator, build_validators, seeded_rng, validator_for_slot,
};
use nimiq_validator::aggregation::view_change::SignedViewChangeMessage;
use nimiq_vrf::VrfSeed;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn one_validator_can_create_micro_blocks() {
    let hub = MockHub::default();
    let env = VolatileEnvironment::new(10).expect("Could not open a volatile database");

    let voting_key = BlsKeyPair::generate(&mut seeded_rng(0));
    let validator_key = KeyPair::generate(&mut seeded_rng(0));
    let fee_key = KeyPair::generate(&mut seeded_rng(0));
    let signing_key = KeyPair::generate(&mut seeded_rng(0));
    let genesis = GenesisBuilder::default()
        .with_genesis_validator(
            Address::from(&validator_key),
            signing_key.public,
            voting_key.public_key,
            Address::default(),
        )
        .generate(env)
        .unwrap();

    let (validator, mut consensus1) = build_validator::<MockNetwork>(
        1,
        Address::from(&validator_key),
        signing_key,
        voting_key,
        fee_key,
        genesis.clone(),
        &mut Some(hub),
    )
    .await;

    log::debug!("Establishing consensus...");
    consensus1.force_established();
    assert_eq!(consensus1.is_established(), true);

    log::debug!("Spawning validator...");
    tokio::spawn(validator);

    let events1 = consensus1.blockchain.write().notifier.as_stream();
    events1.take(10).for_each(|_| future::ready(())).await;

    assert!(consensus1.blockchain.read().block_number() >= 10);
}

#[tokio::test]
async fn four_validators_can_create_micro_blocks() {
    let hub = MockHub::default();
    let env = VolatileEnvironment::new(10).expect("Could not open a volatile database");

    let validators = build_validators::<MockNetwork>(env, 4, &mut Some(hub)).await;

    let blockchain = Arc::clone(&validators.first().unwrap().consensus.blockchain);

    tokio::spawn(future::join_all(validators));

    let events = blockchain.write().notifier.as_stream();
    time::timeout(
        Duration::from_secs(60),
        events.take(30).for_each(|_| future::ready(())),
    )
    .await
    .unwrap();

    assert!(blockchain.read().block_number() >= 30);
}

#[tokio::test]
async fn four_validators_can_view_change() {
    let hub = MockHub::default();
    let env = VolatileEnvironment::new(10).expect("Could not open a volatile database");

    let validators = build_validators::<MockNetwork>(env, 4, &mut Some(hub)).await;

    // Disconnect the next block producer.
    let validator = validator_for_slot(&validators, 1, 0);
    validator.consensus.network.shutdown();

    // Listen for blockchain events from the new block producer (after view change).
    let validator = validator_for_slot(&validators, 1, 1);
    let blockchain = Arc::clone(&validator.consensus.blockchain);
    let mut events = blockchain.write().notifier.as_stream();

    // Freeze time to immediately trigger the view change timeout.
    // time::pause();

    tokio::spawn(future::join_all(validators));

    // Wait for the new block producer to create a block.
    events.next().await;

    assert!(blockchain.read().block_number() >= 1);
    assert!(blockchain.read().view_number() >= 1);
}

fn create_view_change_update(
    block_number: u32,
    new_view_number: u32,
    prev_seed: VrfSeed,
    key_pair: BlsKeyPair,
    validator_id: u16,
    slots: &Vec<u16>,
) -> LevelUpdateMessage<SignedViewChangeMessage, ViewChange> {
    // create view change according to parameters
    let view_change = ViewChange {
        block_number,
        new_view_number,
        vrf_entropy: prev_seed.entropy(),
    };

    // get a single signature for this view_change
    let signed_view_change =
        SignedViewChange::from_message(view_change.clone(), &key_pair.secret_key, validator_id);

    // multiply with number of slots to get a signature representing all the slots of this public_key
    let signature = AggregateSignature::from_signatures(&[signed_view_change
        .signature
        .multiply(slots.len() as u16)]);

    // compute the signers bitset (which is just all the slots)
    let mut signers = BitSet::new();
    for slot in slots {
        signers.insert(*slot as usize);
    }

    // the contribution is composed of the signers bitset with the signature already multiplied by the number of slots.
    let contribution = SignedViewChangeMessage {
        view_change: MultiSignature::new(signature, signers),
        previous_proof: None,
    };

    LevelUpdate::new(
        contribution.clone(),
        Some(contribution),
        1,
        validator_id as usize,
    )
    .with_tag(view_change)
}

#[ignore]
#[tokio::test]
async fn validator_can_catch_up() {
    // remove first block producer in order to trigger a view change. Never connect him again
    // remove the second block producer to trigger another view change after the first one (which we want someone to catch up to). Never connect him again
    // third block producer needs to be disconnected as well and then reconnected to catch up to the seconds view change while not having seen the first one,
    // resulting in him producing the first block.
    let hub = MockHub::default();
    let env = VolatileEnvironment::new(10).expect("Could not open a volatile database");

    // In total 8 validator are registered. after 3 validators are taken offline the remaining 5 should not be able to progress on their own
    let mut validators = build_validators::<MockNetwork>(env, 8, &mut Some(hub)).await;
    // Maintain a collection of the corresponding networks.

    let networks: Vec<Arc<MockNetwork>> = validators
        .iter()
        .map(|v| v.consensus.network.clone())
        .collect();

    // Disconnect the block producers for the next 3 views. remember the one which is supposed to actually create the block (3rd view)
    let (validator, nw) = {
        let validator = validator_for_slot(&mut validators, 1, 0);
        validator.consensus.network.disconnect();
        let id1 = validator.validator_slot_band();
        let validator = validator_for_slot(&mut validators, 1, 1);
        validator.consensus.network.disconnect();
        let id2 = validator.validator_slot_band();
        assert_ne!(id2, id1);

        // ideally we would remove the validators from the vec for them to not even execute.
        // However the implementation does still progress their chains and since they have registered listeners, they would panic.
        // that is confusing, thus they are allowed to execute (with no validator network connection)
        // validators.retain(|v| {
        //     v.validator_address() != id1 && v.validator_address() != id2
        // });

        let validator = validator_for_slot(&validators, 1, 2);
        validator.consensus.network.disconnect();
        assert_ne!(id1, validator.validator_slot_band());
        assert_ne!(id2, validator.validator_slot_band());
        (validator, validator.consensus.network.clone())
    };
    // assert_eq!(validators.len(), 7);

    let blockchain = validator.consensus.blockchain.clone();
    // Listen for blockchain events from the block producer (after two view changes).
    let mut events = blockchain.write().notifier.as_stream();

    let (start, end) = blockchain.read().current_validators().unwrap().validators
        [validator.validator_slot_band() as usize]
        .slot_range;

    let slots = (start..end).collect();

    // Manually construct a view change for the validator
    let vc = create_view_change_update(
        1,
        1,
        blockchain.read().head().seed().clone(),
        validator.voting_key(),
        validator.validator_slot_band(),
        &slots,
    );

    // let the validators run.
    tokio::spawn(future::join_all(validators));

    // while waiting for them to run into the view_change_timeout (10s)
    time::sleep(Duration::from_secs(11)).await;
    // At which point the prepared view_change message is broadcast
    // (only a subset of the validators will accept it as it send as level 1 message)
    for network in &networks {
        network.broadcast(vc.clone()).await;
    }

    // wait enough time to complete the view change (it really does not matter how long, as long as the vc completes)
    time::sleep(Duration::from_secs(8)).await;

    // reconnect a validator (who has not seen the proof for the ViewChange to view 1)
    for network in &networks {
        log::warn!("connecting networks");
        nw.dial_mock(network);
    }

    // Wait for the new block producer to create a blockchainEvent (which is always an extended event for block 1) and keep the hash
    if let Some(BlockchainEvent::Extended(hash)) = events.next().await {
        // retrieve the block for height 1
        if let Some(block) = blockchain.read().get_block_at(1, false, None) {
            // the hash needs to be the one the extended event returned.
            // (the chain itself i.e blockchain.header_hash() might have already progressed further)
            assert_eq!(block.header().hash(), hash);
            // the view of the block needs to be 2 or higher.
            assert!(block.header().view_number() >= 2);
            // now in that case the validator producing this block has progressed the 2nd view change to view 2 without having seen the view change to view 1.
            return;
        }
    }

    assert!(false);
}
