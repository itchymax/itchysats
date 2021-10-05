use anyhow::{bail, Context, Result};
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::{Address, Amount, Network, PrivateKey, PublicKey, Transaction};
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use bdk::wallet::AddressIndex;
use bdk::SignOptions;
use bitcoin::util::psbt::PartiallySignedTransaction;
use cfd_protocol::{
    close_transaction, commit_descriptor, compute_adaptor_pk, create_cfd_transactions,
    finalize_spend_transaction, interval, lock_descriptor, punish_transaction,
    renew_cfd_transactions, spending_tx_sighash, CfdTransactions, Payout, PunishParams,
    TransactionExt, WalletExt,
};
use rand::{thread_rng, CryptoRng, RngCore};
use secp256k1_zkp::{schnorrsig, EcdsaAdaptorSignature, SecretKey, Signature, SECP256K1};
use std::str::FromStr;

#[test]
fn create_cfd() {
    let mut rng = thread_rng();

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let oracle_data = OliviaData::example();
    let oracle_pk = oracle_data.pk;
    let nonce_pks = oracle_data.nonce_pks.clone();

    let payouts = vec![
        Payout::new(
            0..=40_000,
            Amount::from_btc(1.5).unwrap(),
            Amount::from_btc(0.5).unwrap(),
        )
        .unwrap(),
        Payout::new(
            40_001..=70_000,
            Amount::ZERO,
            Amount::from_btc(2.0).unwrap(),
        )
        .unwrap(),
    ]
    .concat();

    let cet_timelock = 0;
    let refund_timelock = 0;

    let (maker_cfd_txs, taker_cfd_txs, maker, taker, maker_addr, taker_addr) = create_cfd_txs(
        &mut rng,
        (&maker_wallet, maker_lock_amount),
        (&taker_wallet, taker_lock_amount),
        (oracle_pk, &nonce_pks),
        payouts,
        (cet_timelock, refund_timelock),
    );

    let lock_desc = lock_descriptor(maker.pk, taker.pk);
    let lock_amount = maker_lock_amount + taker_lock_amount;

    let commit_desc = commit_descriptor(
        (maker.pk, maker.rev_pk, maker.pub_pk),
        (taker.pk, taker.rev_pk, taker.pub_pk),
    );
    let commit_amount = Amount::from_sat(maker_cfd_txs.commit.0.output[0].value);

    verify_cfd_sigs(
        (&maker_cfd_txs, maker.pk, maker.pub_pk),
        (&taker_cfd_txs, taker.pk, taker.pub_pk),
        (oracle_pk, &nonce_pks),
        (&lock_desc, lock_amount),
        (&commit_desc, commit_amount),
    );

    check_cfd_txs(
        (
            maker_wallet,
            maker_cfd_txs,
            maker.sk,
            maker.pk,
            maker.pub_sk,
            maker.pub_pk,
            maker.rev_sk,
            maker_addr,
        ),
        (
            taker_wallet,
            taker_cfd_txs,
            taker.sk,
            taker.pk,
            taker.pub_sk,
            taker.pub_pk,
            taker.rev_sk,
            taker_addr,
        ),
        oracle_data,
        (lock_desc, lock_amount),
        (commit_desc, commit_amount),
    );
}

#[test]
fn renew_cfd() {
    let mut rng = thread_rng();

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let oracle_data = OliviaData::example();
    let oracle_pk = oracle_data.pk;
    let nonce_pks = oracle_data.nonce_pks.clone();

    let payouts = vec![
        Payout::new(0..=10_000, Amount::from_btc(2.0).unwrap(), Amount::ZERO).unwrap(),
        Payout::new(
            10_001..=50_000,
            Amount::ZERO,
            Amount::from_btc(2.0).unwrap(),
        )
        .unwrap(),
    ]
    .concat();

    let cet_timelock = 0;
    let refund_timelock = 0;

    let (maker_cfd_txs, taker_cfd_txs, maker, taker, maker_addr, taker_addr) = create_cfd_txs(
        &mut rng,
        (&maker_wallet, maker_lock_amount),
        (&taker_wallet, taker_lock_amount),
        (oracle_pk, &nonce_pks),
        payouts,
        (cet_timelock, refund_timelock),
    );

    // renew cfd transactions

    let (maker_rev_sk, maker_rev_pk) = make_keypair(&mut rng);
    let (maker_pub_sk, maker_pub_pk) = make_keypair(&mut rng);

    let (taker_rev_sk, taker_rev_pk) = make_keypair(&mut rng);
    let (taker_pub_sk, taker_pub_pk) = make_keypair(&mut rng);

    let payouts = vec![
        Payout::new(
            0..=50_000,
            Amount::from_btc(1.5).unwrap(),
            Amount::from_btc(0.5).unwrap(),
        )
        .unwrap(),
        Payout::new(
            50_001..=70_000,
            Amount::from_btc(0.5).unwrap(),
            Amount::from_btc(1.5).unwrap(),
        )
        .unwrap(),
    ]
    .concat();

    let maker_cfd_txs = renew_cfd_transactions(
        maker_cfd_txs.lock,
        (
            maker.pk,
            maker_lock_amount,
            maker_addr.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker.pk,
            taker_lock_amount,
            taker_addr.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        (oracle_pk, &nonce_pks),
        (cet_timelock, refund_timelock),
        payouts.clone(),
        maker.sk,
    )
    .unwrap();

    let taker_cfd_txs = renew_cfd_transactions(
        taker_cfd_txs.lock,
        (
            maker.pk,
            maker_lock_amount,
            maker_addr.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker.pk,
            taker_lock_amount,
            taker_addr.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        (oracle_pk, &nonce_pks),
        (cet_timelock, refund_timelock),
        payouts,
        taker.sk,
    )
    .unwrap();

    let lock_desc = lock_descriptor(maker.pk, taker.pk);
    let lock_amount = maker_lock_amount + taker_lock_amount;

    let commit_desc = commit_descriptor(
        (maker.pk, maker_rev_pk, maker_pub_pk),
        (taker.pk, taker_rev_pk, taker_pub_pk),
    );
    let commit_amount = Amount::from_sat(maker_cfd_txs.commit.0.output[0].value);

    verify_cfd_sigs(
        (&maker_cfd_txs, maker.pk, maker_pub_pk),
        (&taker_cfd_txs, taker.pk, taker_pub_pk),
        (oracle_pk, &nonce_pks),
        (&lock_desc, lock_amount),
        (&commit_desc, commit_amount),
    );

    check_cfd_txs(
        (
            maker_wallet,
            maker_cfd_txs,
            maker.sk,
            maker.pk,
            maker_pub_sk,
            maker_pub_pk,
            maker_rev_sk,
            maker_addr,
        ),
        (
            taker_wallet,
            taker_cfd_txs,
            taker.sk,
            taker.pk,
            taker_pub_sk,
            taker_pub_pk,
            taker_rev_sk,
            taker_addr,
        ),
        oracle_data,
        (lock_desc, lock_amount),
        (commit_desc, commit_amount),
    )
}

#[test]
fn collaboratively_close_cfd() {
    let mut rng = thread_rng();

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let oracle_data = OliviaData::example();
    let oracle_pk = oracle_data.pk;
    let nonce_pks = oracle_data.nonce_pks;

    let payouts = vec![Payout::new(
        0..=100_000,
        Amount::from_btc(1.5).unwrap(),
        Amount::from_btc(0.5).unwrap(),
    )
    .unwrap()]
    .concat();

    let cet_timelock = 0;
    let refund_timelock = 0;

    let (maker_cfd_txs, _, maker, taker, maker_addr, taker_addr) = create_cfd_txs(
        &mut rng,
        (&maker_wallet, maker_lock_amount),
        (&taker_wallet, taker_lock_amount),
        (oracle_pk, &nonce_pks),
        payouts,
        (cet_timelock, refund_timelock),
    );

    let lock_tx = maker_cfd_txs.lock.extract_tx();
    let lock_desc = lock_descriptor(maker.pk, taker.pk);
    let (lock_outpoint, lock_amount) = {
        let outpoint = lock_tx
            .outpoint(&lock_desc.script_pubkey())
            .expect("lock script to be in lock tx");
        let amount = Amount::from_sat(lock_tx.output[outpoint.vout as usize].value);

        (outpoint, amount)
    };

    let maker_amount = Amount::ONE_BTC;
    let taker_amount = Amount::ONE_BTC;

    let (close_tx, close_sighash) = close_transaction(
        &lock_desc,
        lock_outpoint,
        lock_amount,
        (&maker_addr, maker_amount),
        (&taker_addr, taker_amount),
    )
    .expect("to build close tx");

    let sig_maker = SECP256K1.sign(&close_sighash, &maker.sk);
    let sig_taker = SECP256K1.sign(&close_sighash, &taker.sk);
    let signed_close_tx = finalize_spend_transaction(
        close_tx,
        &lock_desc,
        (maker.pk, sig_maker),
        (taker.pk, sig_taker),
    )
    .expect("to sign close tx");

    check_tx(&lock_tx, &signed_close_tx, &lock_desc).expect("valid close tx");
}

fn create_cfd_txs(
    rng: &mut (impl RngCore + CryptoRng),
    (maker_wallet, maker_lock_amount): (&bdk::Wallet<(), bdk::database::MemoryDatabase>, Amount),
    (taker_wallet, taker_lock_amount): (&bdk::Wallet<(), bdk::database::MemoryDatabase>, Amount),
    (oracle_pk, nonce_pks): (schnorrsig::PublicKey, &[schnorrsig::PublicKey]),
    payouts: Vec<Payout>,
    (cet_timelock, refund_timelock): (u32, u32),
) -> (
    CfdTransactions,
    CfdTransactions,
    CfdKeys,
    CfdKeys,
    Address,
    Address,
) {
    let (maker_sk, maker_pk) = make_keypair(rng);
    let (taker_sk, taker_pk) = make_keypair(rng);

    let maker_addr = maker_wallet.get_address(AddressIndex::New).unwrap();
    let taker_addr = taker_wallet.get_address(AddressIndex::New).unwrap();

    let (maker_rev_sk, maker_rev_pk) = make_keypair(rng);
    let (maker_pub_sk, maker_pub_pk) = make_keypair(rng);
    let (taker_rev_sk, taker_rev_pk) = make_keypair(rng);
    let (taker_pub_sk, taker_pub_pk) = make_keypair(rng);
    let maker_params = maker_wallet
        .build_party_params(maker_lock_amount, maker_pk)
        .unwrap();
    let taker_params = taker_wallet
        .build_party_params(taker_lock_amount, taker_pk)
        .unwrap();
    let maker_cfd_txs = create_cfd_transactions(
        (
            maker_params.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker_params.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        (oracle_pk, nonce_pks),
        (cet_timelock, refund_timelock),
        payouts.clone(),
        maker_sk,
    )
    .unwrap();
    let taker_cfd_txs = create_cfd_transactions(
        (
            maker_params,
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker_params,
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        (oracle_pk, nonce_pks),
        (cet_timelock, refund_timelock),
        payouts,
        taker_sk,
    )
    .unwrap();
    (
        maker_cfd_txs,
        taker_cfd_txs,
        CfdKeys {
            sk: maker_sk,
            pk: maker_pk,
            rev_sk: maker_rev_sk,
            rev_pk: maker_rev_pk,
            pub_sk: maker_pub_sk,
            pub_pk: maker_pub_pk,
        },
        CfdKeys {
            sk: taker_sk,
            pk: taker_pk,
            rev_sk: taker_rev_sk,
            rev_pk: taker_rev_pk,
            pub_sk: taker_pub_sk,
            pub_pk: taker_pub_pk,
        },
        maker_addr.address,
        taker_addr.address,
    )
}

struct CfdKeys {
    sk: SecretKey,
    pk: PublicKey,
    rev_sk: SecretKey,
    rev_pk: PublicKey,
    pub_sk: SecretKey,
    pub_pk: PublicKey,
}

fn verify_cfd_sigs(
    (maker_cfd_txs, maker_pk, maker_publish_pk): (&CfdTransactions, PublicKey, PublicKey),
    (taker_cfd_txs, taker_pk, taker_publish_pk): (&CfdTransactions, PublicKey, PublicKey),
    (oracle_pk, nonce_pks): (schnorrsig::PublicKey, &[schnorrsig::PublicKey]),
    (lock_desc, lock_amount): (&Descriptor<PublicKey>, Amount),
    (commit_desc, commit_amount): (&Descriptor<PublicKey>, Amount),
) {
    verify_spend(
        &taker_cfd_txs.refund.0,
        &maker_cfd_txs.refund.1,
        commit_desc,
        commit_amount,
        &maker_pk.key,
    )
    .expect("valid maker refund sig");
    verify_spend(
        &maker_cfd_txs.refund.0,
        &taker_cfd_txs.refund.1,
        commit_desc,
        commit_amount,
        &taker_pk.key,
    )
    .expect("valid taker refund sig");
    for (tx, _, digits) in taker_cfd_txs.cets.iter() {
        maker_cfd_txs
            .cets
            .iter()
            .find(|(maker_tx, maker_encsig, _)| {
                maker_tx.txid() == tx.txid()
                    && verify_cet_encsig(
                        tx,
                        maker_encsig,
                        digits,
                        &maker_pk.key,
                        (oracle_pk, nonce_pks),
                        commit_desc,
                        commit_amount,
                    )
                    .is_ok()
            })
            .expect("one valid maker cet encsig per cet");
    }
    for (tx, _, msg_nonce_pairs) in maker_cfd_txs.cets.iter() {
        taker_cfd_txs
            .cets
            .iter()
            .find(|(taker_tx, taker_encsig, _)| {
                taker_tx.txid() == tx.txid()
                    && verify_cet_encsig(
                        tx,
                        taker_encsig,
                        msg_nonce_pairs,
                        &taker_pk.key,
                        (oracle_pk, nonce_pks),
                        commit_desc,
                        commit_amount,
                    )
                    .is_ok()
            })
            .expect("one valid taker cet encsig per cet");
    }
    encverify_spend(
        &taker_cfd_txs.commit.0,
        &maker_cfd_txs.commit.1,
        lock_desc,
        lock_amount,
        &taker_publish_pk.key,
        &maker_pk.key,
    )
    .expect("valid maker commit encsig");
    encverify_spend(
        &maker_cfd_txs.commit.0,
        &taker_cfd_txs.commit.1,
        lock_desc,
        lock_amount,
        &maker_publish_pk.key,
        &taker_pk.key,
    )
    .expect("valid taker commit encsig");
}

fn check_cfd_txs(
    (
        maker_wallet,
        maker_cfd_txs,
        maker_sk,
        maker_pk,
        maker_pub_sk,
        maker_pub_pk,
        maker_rev_sk,
        maker_addr,
    ): (
        bdk::Wallet<(), bdk::database::MemoryDatabase>,
        CfdTransactions,
        SecretKey,
        PublicKey,
        SecretKey,
        PublicKey,
        SecretKey,
        Address,
    ),
    (
        taker_wallet,
        taker_cfd_txs,
        taker_sk,
        taker_pk,
        taker_pub_sk,
        taker_pub_pk,
        taker_rev_sk,
        taker_addr,
    ): (
        bdk::Wallet<(), bdk::database::MemoryDatabase>,
        CfdTransactions,
        SecretKey,
        PublicKey,
        SecretKey,
        PublicKey,
        SecretKey,
        Address,
    ),
    oracle_data: OliviaData,
    (lock_desc, lock_amount): (Descriptor<PublicKey>, Amount),
    (commit_desc, commit_amount): (Descriptor<PublicKey>, Amount),
) {
    // Lock transaction (either party can do this):

    let signed_lock_tx = sign_lock_tx(maker_cfd_txs.lock.clone(), maker_wallet, taker_wallet)
        .expect("to build signed lock tx");

    // Commit transactions:

    let signed_commit_tx_maker = decrypt_and_sign(
        maker_cfd_txs.commit.0.clone(),
        (&maker_sk, &maker_pk),
        &maker_pub_sk,
        &taker_pk,
        &taker_cfd_txs.commit.1,
        &lock_desc,
        lock_amount,
    )
    .expect("maker to build signed commit tx");
    check_tx(&signed_lock_tx, &signed_commit_tx_maker, &lock_desc).expect("valid maker commit tx");
    let signed_commit_tx_taker = decrypt_and_sign(
        taker_cfd_txs.commit.0.clone(),
        (&taker_sk, &taker_pk),
        &taker_pub_sk,
        &maker_pk,
        &maker_cfd_txs.commit.1,
        &lock_desc,
        lock_amount,
    )
    .expect("taker to build signed commit tx");
    check_tx(&signed_lock_tx, &signed_commit_tx_taker, &lock_desc).expect("valid taker commit tx");

    // Refund transaction (both parties would produce the same one):

    let signed_refund_tx = finalize_spend_transaction(
        maker_cfd_txs.refund.0.clone(),
        &commit_desc,
        (maker_pk, maker_cfd_txs.refund.1),
        (taker_pk, taker_cfd_txs.refund.1),
    )
    .expect("to build signed refund tx");
    check_tx(&signed_commit_tx_maker, &signed_refund_tx, &commit_desc).expect("valid refund tx");

    // CETs:

    let unlocked_cets = maker_cfd_txs.cets.clone().into_iter().filter_map({
        |(tx, _, digits)| {
            let price = oracle_data.price;
            if !digits.range().contains(&price) {
                return None;
            }

            let oracle_attestations = oracle_data.attestations.clone();
            build_and_check_cet(
                tx,
                &taker_cfd_txs.cets,
                (&maker_sk, &maker_pk),
                &taker_pk,
                (price, &oracle_attestations),
                (&signed_commit_tx_maker, &commit_desc, commit_amount),
            )
            .expect("valid maker cet");

            Some(())
        }
    });
    assert_eq!(unlocked_cets.count(), 1, "Expected to unlock only 1 CET");

    let unlocked_cets = taker_cfd_txs
        .cets
        .into_iter()
        .filter_map(|(tx, _, digits)| {
            let price = oracle_data.price;
            if !digits.range().contains(&price) {
                return None;
            }

            build_and_check_cet(
                tx,
                &maker_cfd_txs.cets,
                (&taker_sk, &taker_pk),
                &maker_pk,
                (price, &oracle_data.attestations),
                (&signed_commit_tx_maker, &commit_desc, commit_amount),
            )
            .expect("valid taker cet");

            Some(())
        });
    assert_eq!(unlocked_cets.count(), 1, "Expected to unlock only 1 CET");

    // Punish transactions:

    let punish_tx_maker = punish_transaction(
        &commit_desc,
        &maker_addr,
        maker_cfd_txs.commit.1,
        maker_sk,
        taker_rev_sk,
        taker_pub_pk,
        &signed_commit_tx_taker,
    )
    .expect("maker to build punish tx");
    check_tx(&signed_commit_tx_taker, &punish_tx_maker, &commit_desc)
        .expect("valid maker punish tx");
    let punish_tx_taker = punish_transaction(
        &commit_desc,
        &taker_addr,
        taker_cfd_txs.commit.1,
        taker_sk,
        maker_rev_sk,
        maker_pub_pk,
        &signed_commit_tx_maker,
    )
    .expect("taker to build punish tx");
    check_tx(&signed_commit_tx_maker, &punish_tx_taker, &commit_desc)
        .expect("valid taker punish tx");
}

fn build_and_check_cet(
    cet: Transaction,
    cets_other: &[(Transaction, EcdsaAdaptorSignature, interval::Digits)],
    (sk, pk): (&SecretKey, &PublicKey),
    pk_other: &PublicKey,
    (price, oracle_attestations): (u64, &[SecretKey]),
    (commit_tx, commit_desc, commit_amount): (&Transaction, &Descriptor<PublicKey>, Amount),
) -> Result<()> {
    let (encsig_other, n_bits) = cets_other
        .iter()
        .find_map(|(_, encsig, digits)| {
            (digits.range().contains(&price)).then(|| (encsig, digits.len()))
        })
        .expect("one encsig per cet, per party");

    let (oracle_attestations, _) = oracle_attestations.split_at(n_bits);

    let mut decryption_sk = oracle_attestations[0];
    for oracle_attestation in oracle_attestations[1..].iter() {
        decryption_sk.add_assign(oracle_attestation.as_ref())?;
    }

    let signed_cet = decrypt_and_sign(
        cet,
        (sk, pk),
        &decryption_sk,
        pk_other,
        encsig_other,
        commit_desc,
        commit_amount,
    )
    .context("failed to build signed cet")?;
    check_tx(commit_tx, &signed_cet, commit_desc).context("invalid cet")?;

    Ok(())
}

fn check_tx(
    spent_tx: &Transaction,
    spend_tx: &Transaction,
    spent_descriptor: &Descriptor<PublicKey>,
) -> Result<()> {
    let spent_script_pubkey = spent_descriptor.script_pubkey();
    let spent_outpoint = spent_tx
        .outpoint(&spent_script_pubkey)
        .context("spend tx doesn't spend from spent tx")?;
    let spent_amount = spent_tx.output[spent_outpoint.vout as usize].value;

    check_tx_fee(&[spent_tx], spend_tx)?;
    spent_descriptor.script_pubkey().verify(
        0,
        spent_amount,
        bitcoin::consensus::serialize(spend_tx).as_slice(),
    )?;

    Ok(())
}

fn decrypt_and_sign(
    spend_tx: Transaction,
    (sk, pk): (&SecretKey, &PublicKey),
    decryption_sk: &SecretKey,
    pk_other: &PublicKey,
    encsig_other: &EcdsaAdaptorSignature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
) -> Result<Transaction> {
    let sighash = spending_tx_sighash(&spend_tx, spent_descriptor, spent_amount);

    let sig_self = SECP256K1.sign(&sighash, sk);

    encsig_other
        .verify(
            SECP256K1,
            &sighash,
            &pk_other.key,
            &secp256k1_zkp::PublicKey::from_secret_key(SECP256K1, decryption_sk),
        )
        .expect("wrong decryption key");
    let sig_other = encsig_other.decrypt(decryption_sk)?;

    let signed_commit_tx = finalize_spend_transaction(
        spend_tx,
        spent_descriptor,
        (*pk, sig_self),
        (*pk_other, sig_other),
    )?;

    Ok(signed_commit_tx)
}

fn sign_lock_tx(
    mut lock_tx: PartiallySignedTransaction,
    maker_wallet: bdk::Wallet<(), bdk::database::MemoryDatabase>,
    taker_wallet: bdk::Wallet<(), bdk::database::MemoryDatabase>,
) -> Result<Transaction> {
    maker_wallet
        .sign(
            &mut lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .context("maker could not sign lock tx")?;
    taker_wallet
        .sign(
            &mut lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .context("taker could not sign lock tx")?;

    Ok(lock_tx.extract_tx())
}

fn verify_spend(
    tx: &Transaction,
    sig: &Signature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    pk: &secp256k1_zkp::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);
    SECP256K1
        .verify(&sighash, sig, pk)
        .context("failed to verify sig on spend tx")
}

fn verify_cet_encsig(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    digits: &interval::Digits,
    pk: &secp256k1_zkp::PublicKey,
    (oracle_pk, nonce_pks): (schnorrsig::PublicKey, &[schnorrsig::PublicKey]),
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
) -> Result<()> {
    let index_nonce_pairs = &digits
        .to_indices()
        .into_iter()
        .zip(nonce_pks.iter().cloned())
        .collect::<Vec<_>>();

    let adaptor_point = compute_adaptor_pk(&oracle_pk, index_nonce_pairs)
        .context("could not calculate adaptor point")?;
    encverify_spend(
        tx,
        encsig,
        spent_descriptor,
        spent_amount,
        &adaptor_point,
        pk,
    )
}

fn encverify_spend(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    encryption_point: &secp256k1_zkp::PublicKey,
    pk: &secp256k1_zkp::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);
    encsig
        .verify(SECP256K1, &sighash, pk, encryption_point)
        .context("failed to verify encsig spend tx")
}

fn check_tx_fee(input_txs: &[&Transaction], spend_tx: &Transaction) -> Result<()> {
    let input_amount = spend_tx
        .input
        .iter()
        .try_fold::<_, _, Result<_>>(0, |acc, input| {
            let value = input_txs
                .iter()
                .find_map(|tx| {
                    (tx.txid() == input.previous_output.txid)
                        .then(|| tx.output[input.previous_output.vout as usize].value)
                })
                .with_context(|| {
                    format!(
                        "spend tx input {} not found in input_txs",
                        input.previous_output
                    )
                })?;

            Ok(acc + value)
        })?;

    let output_amount = spend_tx
        .output
        .iter()
        .fold(0, |acc, output| acc + output.value);
    let fee = input_amount - output_amount;

    let min_relay_fee = spend_tx.get_virtual_size();
    if (fee as f64) < min_relay_fee {
        bail!("min relay fee not met, {} < {}", fee, min_relay_fee)
    }

    Ok(())
}

fn build_wallet(
    rng: &mut (impl RngCore + CryptoRng),
    utxo_amount: Amount,
    num_utxos: u8,
) -> Result<bdk::Wallet<(), bdk::database::MemoryDatabase>> {
    use bdk::{populate_test_db, testutils};

    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);

    let key = ExtendedPrivKey::new_master(Network::Regtest, &seed)?;
    let descriptors = testutils!(@descriptors (&format!("wpkh({}/*)", key)));

    let mut database = bdk::database::MemoryDatabase::new();

    for index in 0..num_utxos {
        populate_test_db!(
            &mut database,
            testutils! {
                @tx ( (@external descriptors, index as u32) => utxo_amount.as_sat() ) (@confirmations 1)
            },
            Some(100)
        );
    }

    let wallet = bdk::Wallet::new_offline(&descriptors.0, None, Network::Regtest, database)?;

    Ok(wallet)
}

fn make_keypair(rng: &mut (impl RngCore + CryptoRng)) -> (SecretKey, PublicKey) {
    let sk = SecretKey::new(rng);
    let pk = PublicKey::from_private_key(
        SECP256K1,
        &PrivateKey {
            compressed: true,
            network: Network::Regtest,
            key: sk,
        },
    );

    (sk, pk)
}

struct OliviaData {
    pk: schnorrsig::PublicKey,
    nonce_pks: Vec<schnorrsig::PublicKey>,
    price: u64,
    attestations: Vec<SecretKey>,
}

impl OliviaData {
    /// An example of all the data necessary from `olivia` to test the
    /// CFD protocol.
    ///
    /// Data comes from this event:
    /// https://outcome.observer/h00.ooo/x/BitMEX/BXBT/2021-10-05T02:00:00.price[n:20].
    pub fn example() -> Self {
        let pk = schnorrsig::PublicKey::from_str(
            "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
        )
        .unwrap();

        let nonce_pks = [
            "d02d163cf9623f567c4e3faf851a9266ac1ede13da4ca4141f3a7717fba9a739",
            "bc310f26aa5addbc382f653d8530aaead7c25e3546abc24639f490e36d4bdb88",
            "2661375f570dcc32300d442e85b6d72dfa3232dccda45e8fb4a2d1e758d1d374",
            "fcc68fbf071d391b14c0867cb4defb5a8abc12418dff3dfc2f84fd4025cb2716",
            "cf5c2b7fe3851c64a7ff9635a9bfc50cdd301401d002f2da049f4c6a20e8457b",
            "14f1005d8c2832a2c4666dd732dd9bb3af9c8f70ebcdaec96869b1ca0c8e0de6",
            "299ee1c9c20fab8b067adf452a7d7661b5e7f5dd6bc707562805002e7cb8443e",
            "bcb4e5a594346de298993a7a31762f598b5224b977e23182369e9ed3e5127f78",
            "25e09a16ee5d469069abfb62cd5e1f20af50cf15241f571e64fa28b127304574",
            "3ed5a1422f43299caf281123aba88bd4bc61ec863f4afb79c7ce7663ad44db5d",
            "a7e0f61212735c192c4bf16e0a3e925e65f9f3feb6f1e5e8d6f5c18cf2dbb5a8",
            "a36a631015d9036d0c321fea7cf12f589aa196e7279b4a290de5112c2940e540",
            "b5bdd931f81970139e7301ac654b378077c3ed993ca7893ed93fee5fc6f7a782",
            "00090816e256b41e042dce38bde99ab3cf9482f9b066836988d3ed54833638e8",
            "3530408e93c251f5f488d3b1c608157177c459d6fab1966abebf765bcc9338d2",
            "603269ce88d112ff7fcfcaab82f228be97deca37f8190084d509c71b51a30432",
            "f0587414fcc6c56aef11d4a1d287ad6b55b237c5b8a5d5d93eb9ca06f6466ccf",
            "763009afb0ffd99c7b835488cb3b0302f3b78f59bbfd5292bedab8ef9da8c1b7",
            "3867af9048309a05004a164bdea09899f23ff1d83b6491b2b53a1b7b92e0eb2e",
            "688118e6b59e27944c277513db2711a520f4283c7c53a11f58d9f6a46d82c964",
        ]
        .iter()
        .map(|pk| schnorrsig::PublicKey::from_str(pk).unwrap())
        .collect();

        let attestations = [
            "5bc7663195971daaa1e3e6a81b4bca65882791644bc446fc060cbc118a3ace0f",
            "721d0cb56a0778a1ca7907f81a0787f34385b13f854c845c4c5539f7f6267958",
            "044aeef0d525c8ff48758c80939e95807bc640990cc03f53ab6fc0b262045221",
            "79f5175423ec6ee69c8d0e55251db85f3015c2edfa5a03095443fbbf35eb2282",
            "233b9ec549e9cc7c702109d29636db85a3ec63a66f3b53444bcc7586d36ca439",
            "2961a00320b7c9a70220060019a6ca88e18c205fadd2f873c174e5ccbbed527e",
            "bdb76e8f81c39ade4205ead9b68118757fc49ec22769605f26ef904b235283d6",
            "6e75dafedf4ed685513ec1f5c93508de4fad2be05b46001ac00c03474f4690e1",
            "cfcfc27eb9273b343b3042f0386e77efe329066be079788bb00ab47d72f26780",
            "2d931ffd2963e74566365674583abc427bdb6ae571c4887d81f1920f0850665d",
            "33b6f1112fa046cbc04be44c615e70519702662c1f72d8d49b3c4613614a8a46",
            "19e569b15410fa9a758c1a6c211eae8c1547efbe0ac6a7709902be93415f2f09",
            "d859dd5c9a58e1836d1eea3ebe7f48198a681d29e5a5cd6922532d2e94a53a1d",
            "3387eb2ad5e64cd102167766bb72b447f4a2e5129d161e422f9d41cd7d1cc281",
            "db35a9778a1e3abc8d8ab2f4a79346ae2154c9e0b4932d859d1f3e244f67ae76",
            "c3be969e8b889cfb2ece71123e6be5538a2d3a1229637b18bccc179073c38059",
            "6f73263f430e10b82d0fd06c4ddd3b8a6b58c3e756745bd0d9e71a399e517921",
            "0818c9c245d7d2162cd393c562a121f80405a27d22ae465e95030c31ebb4bd24",
            "b7c03f0bd6d63bd78ad4ea0f3452ff9717ba65ca42038e6e90a1aa558b7942dc",
            "90c4d8ec9f408ccb62a62daa993c20f2f86799e1fdea520c6d060418e55fd216",
        ]
        .iter()
        .map(|pk| SecretKey::from_str(pk).unwrap())
        .collect();

        Self {
            pk,
            nonce_pks,
            attestations,
            price: 49262,
        }
    }
}
