//! Tools for scanning a compact representation of the Zcash block chain.

use ff::PrimeField;
use std::collections::HashSet;
use subtle::{ConditionallySelectable, ConstantTimeEq, CtOption};
use zcash_primitives::{
    consensus::{self, BlockHeight},
    merkle_tree::{CommitmentTree, IncrementalWitness},
    note_encryption::try_sapling_compact_note_decryption,
    primitives::Nullifier,
    sapling::Node,
    transaction::TxId,
};

use crate::proto::compact_formats::{CompactBlock, CompactOutput};
use crate::wallet::{AccountId, WalletShieldedOutput, WalletShieldedSpend, WalletTx};

/// Scans a [`CompactOutput`] with a set of [`ExtendedFullViewingKey`]s.
///
/// Returns a [`WalletShieldedOutput`] and corresponding [`IncrementalWitness`] if this
/// output belongs to any of the given [`ExtendedFullViewingKey`]s.
///
/// The given [`CommitmentTree`] and existing [`IncrementalWitness`]es are incremented
/// with this output's commitment.
fn scan_output<P: consensus::Parameters>(
    params: &P,
    height: BlockHeight,
    (index, output): (usize, CompactOutput),
    ivks: &[(AccountId, jubjub::Fr)],
    spent_from_accounts: &HashSet<AccountId>,
    tree: &mut CommitmentTree<Node>,
    existing_witnesses: &mut [&mut IncrementalWitness<Node>],
    block_witnesses: &mut [&mut IncrementalWitness<Node>],
    new_witnesses: &mut [&mut IncrementalWitness<Node>],
) -> Option<WalletShieldedOutput> {
    let cmu = output.cmu().ok()?;
    let epk = output.epk().ok()?;
    let ct = output.ciphertext;

    // Increment tree and witnesses
    let node = Node::new(cmu.to_repr());
    for witness in existing_witnesses {
        witness.append(node).unwrap();
    }
    for witness in block_witnesses {
        witness.append(node).unwrap();
    }
    for witness in new_witnesses {
        witness.append(node).unwrap();
    }
    tree.append(node).unwrap();

    for (account, ivk) in ivks.iter() {
        let (note, to) =
            match try_sapling_compact_note_decryption(params, height, ivk, &epk, &cmu, &ct) {
                Some(ret) => ret,
                None => continue,
            };

        // A note is marked as "change" if the account that received it
        // also spent notes in the same transaction. This will catch,
        // for instance:
        // - Change created by spending fractions of notes.
        // - Notes created by consolidation transactions.
        // - Notes sent from one account to itself.
        let is_change = spent_from_accounts.contains(&account);

        return Some(WalletShieldedOutput {
            index,
            cmu,
            epk,
            account: *account,
            note,
            to,
            is_change,
            witness: IncrementalWitness::from_tree(tree),
        });
    }
    None
}

/// Scans a [`CompactBlock`] with a set of [`ExtendedFullViewingKey`]s.
///
/// Returns a vector of [`WalletTx`]s belonging to any of the given
/// [`ExtendedFullViewingKey`]s, and the corresponding new [`IncrementalWitness`]es.
///
/// The given [`CommitmentTree`] and existing [`IncrementalWitness`]es are
/// incremented appropriately.
pub fn scan_block<P: consensus::Parameters>(
    params: &P,
    block: CompactBlock,
    ivks: &[(AccountId, jubjub::Fr)],
    nullifiers: &[(AccountId, Nullifier)],
    tree: &mut CommitmentTree<Node>,
    existing_witnesses: &mut [&mut IncrementalWitness<Node>],
) -> Vec<WalletTx> {
    let mut wtxs: Vec<WalletTx> = vec![];
    let block_height = block.height();

    for tx in block.vtx.into_iter() {
        let num_spends = tx.spends.len();
        let num_outputs = tx.outputs.len();

        // Check for spent notes
        // The only step that is not constant-time is the filter() at the end.
        let shielded_spends: Vec<_> = tx
            .spends
            .into_iter()
            .enumerate()
            .map(|(index, spend)| {
                let spend_nf = spend.nf().expect(
                    "Could not deserialize nullifier for spend from protobuf representation.",
                );
                // Find the first tracked nullifier that matches this spend, and produce
                // a WalletShieldedSpend if there is a match, in constant time.
                nullifiers
                    .iter()
                    .map(|&(account, nf)| CtOption::new(account, nf.ct_eq(&spend_nf)))
                    .fold(
                        CtOption::new(AccountId::default(), 0.into()),
                        |first, next| CtOption::conditional_select(&next, &first, first.is_some()),
                    )
                    .map(|account| WalletShieldedSpend {
                        index,
                        nf: spend_nf,
                        account,
                    })
            })
            .filter(|spend| spend.is_some().into())
            .map(|spend| spend.unwrap())
            .collect();

        // Collect the set of accounts that were spent from in this transaction
        let spent_from_accounts: HashSet<_> =
            shielded_spends.iter().map(|spend| spend.account).collect();

        // Check for incoming notes while incrementing tree and witnesses
        let mut shielded_outputs: Vec<WalletShieldedOutput> = vec![];
        {
            // Grab mutable references to new witnesses from previous transactions
            // in this block so that we can update them. Scoped so we don't hold
            // mutable references to wtxs for too long.
            let mut block_witnesses: Vec<_> = wtxs
                .iter_mut()
                .flat_map(|tx| {
                    tx.shielded_outputs
                        .iter_mut()
                        .map(|output| &mut output.witness)
                })
                .collect();

            for to_scan in tx.outputs.into_iter().enumerate() {
                // Grab mutable references to new witnesses from previous outputs
                // in this transaction so that we can update them. Scoped so we
                // don't hold mutable references to shielded_outputs for too long.
                let mut new_witnesses: Vec<_> = shielded_outputs
                    .iter_mut()
                    .map(|output| &mut output.witness)
                    .collect();

                if let Some(output) = scan_output(
                    params,
                    block_height,
                    to_scan,
                    &ivks,
                    &spent_from_accounts,
                    tree,
                    existing_witnesses,
                    &mut block_witnesses,
                    &mut new_witnesses,
                ) {
                    shielded_outputs.push(output);
                }
            }
        }

        if !(shielded_spends.is_empty() && shielded_outputs.is_empty()) {
            let mut txid = TxId([0u8; 32]);
            txid.0.copy_from_slice(&tx.hash);
            wtxs.push(WalletTx {
                txid,
                index: tx.index as usize,
                num_spends,
                num_outputs,
                shielded_spends,
                shielded_outputs,
            });
        }
    }

    wtxs
}

#[cfg(test)]
mod tests {
    use ff::{Field, PrimeField};
    use group::GroupEncoding;
    use rand_core::{OsRng, RngCore};
    use zcash_primitives::{
        consensus::{BlockHeight, Network},
        constants::SPENDING_KEY_GENERATOR,
        merkle_tree::CommitmentTree,
        note_encryption::{Memo, SaplingNoteEncryption},
        primitives::{Note, Nullifier},
        transaction::components::Amount,
        util::generate_random_rseed,
        zip32::{ExtendedFullViewingKey, ExtendedSpendingKey},
    };

    use super::scan_block;
    use crate::proto::compact_formats::{CompactBlock, CompactOutput, CompactSpend, CompactTx};
    use crate::wallet::AccountId;

    fn random_compact_tx(mut rng: impl RngCore) -> CompactTx {
        let fake_nf = {
            let mut nf = vec![0; 32];
            rng.fill_bytes(&mut nf);
            nf
        };
        let fake_cmu = {
            let fake_cmu = bls12_381::Scalar::random(&mut rng);
            fake_cmu.to_repr().as_ref().to_owned()
        };
        let fake_epk = {
            let mut buffer = [0; 64];
            rng.fill_bytes(&mut buffer);
            let fake_esk = jubjub::Fr::from_bytes_wide(&buffer);
            let fake_epk = SPENDING_KEY_GENERATOR * fake_esk;
            fake_epk.to_bytes().to_vec()
        };
        let mut cspend = CompactSpend::new();
        cspend.set_nf(fake_nf);
        let mut cout = CompactOutput::new();
        cout.set_cmu(fake_cmu);
        cout.set_epk(fake_epk);
        cout.set_ciphertext(vec![0; 52]);
        let mut ctx = CompactTx::new();
        let mut txid = vec![0; 32];
        rng.fill_bytes(&mut txid);
        ctx.set_hash(txid);
        ctx.spends.push(cspend);
        ctx.outputs.push(cout);
        ctx
    }

    /// Create a fake CompactBlock at the given height, with a transaction containing a
    /// single spend of the given nullifier and a single output paying the given address.
    /// Returns the CompactBlock.
    fn fake_compact_block(
        height: BlockHeight,
        nf: Nullifier,
        extfvk: ExtendedFullViewingKey,
        value: Amount,
        tx_after: bool,
    ) -> CompactBlock {
        let to = extfvk.default_address().unwrap().1;

        // Create a fake Note for the account
        let mut rng = OsRng;
        let rseed = generate_random_rseed(&Network::TestNetwork, height, &mut rng);
        let note = Note {
            g_d: to.diversifier().g_d().unwrap(),
            pk_d: to.pk_d().clone(),
            value: value.into(),
            rseed,
        };
        let encryptor = SaplingNoteEncryption::new(
            Some(extfvk.fvk.ovk),
            note.clone(),
            to.clone(),
            Memo::default(),
            &mut rng,
        );
        let cmu = note.cmu().to_repr().as_ref().to_owned();
        let epk = encryptor.epk().to_bytes().to_vec();
        let enc_ciphertext = encryptor.encrypt_note_plaintext();

        // Create a fake CompactBlock containing the note
        let mut cb = CompactBlock::new();
        cb.set_height(height.into());

        // Add a random Sapling tx before ours
        {
            let mut tx = random_compact_tx(&mut rng);
            tx.index = cb.vtx.len() as u64;
            cb.vtx.push(tx);
        }

        let mut cspend = CompactSpend::new();
        cspend.set_nf(nf.0.to_vec());
        let mut cout = CompactOutput::new();
        cout.set_cmu(cmu);
        cout.set_epk(epk);
        cout.set_ciphertext(enc_ciphertext[..52].to_vec());
        let mut ctx = CompactTx::new();
        let mut txid = vec![0; 32];
        rng.fill_bytes(&mut txid);
        ctx.set_hash(txid);
        ctx.spends.push(cspend);
        ctx.outputs.push(cout);
        ctx.index = cb.vtx.len() as u64;
        cb.vtx.push(ctx);

        // Optionally add another random Sapling tx after ours
        if tx_after {
            let mut tx = random_compact_tx(&mut rng);
            tx.index = cb.vtx.len() as u64;
            cb.vtx.push(tx);
        }

        cb
    }

    #[test]
    fn scan_block_with_my_tx() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);

        let cb = fake_compact_block(
            1u32.into(),
            Nullifier([0; 32]),
            extfvk.clone(),
            Amount::from_u64(5).unwrap(),
            false,
        );
        assert_eq!(cb.vtx.len(), 2);

        let mut tree = CommitmentTree::new();
        let txs = scan_block(
            &Network::TestNetwork,
            cb,
            &[(AccountId(0), extfvk.fvk.vk.ivk())],
            &[],
            &mut tree,
            &mut [],
        );
        assert_eq!(txs.len(), 1);

        let tx = &txs[0];
        assert_eq!(tx.index, 1);
        assert_eq!(tx.num_spends, 1);
        assert_eq!(tx.num_outputs, 1);
        assert_eq!(tx.shielded_spends.len(), 0);
        assert_eq!(tx.shielded_outputs.len(), 1);
        assert_eq!(tx.shielded_outputs[0].index, 0);
        assert_eq!(tx.shielded_outputs[0].account, AccountId(0));
        assert_eq!(tx.shielded_outputs[0].note.value, 5);

        // Check that the witness root matches
        assert_eq!(tx.shielded_outputs[0].witness.root(), tree.root());
    }

    #[test]
    fn scan_block_with_txs_after_my_tx() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);

        let cb = fake_compact_block(
            1u32.into(),
            Nullifier([0; 32]),
            extfvk.clone(),
            Amount::from_u64(5).unwrap(),
            true,
        );
        assert_eq!(cb.vtx.len(), 3);

        let mut tree = CommitmentTree::new();
        let txs = scan_block(
            &Network::TestNetwork,
            cb,
            &[(AccountId(0), extfvk.fvk.vk.ivk())],
            &[],
            &mut tree,
            &mut [],
        );
        assert_eq!(txs.len(), 1);

        let tx = &txs[0];
        assert_eq!(tx.index, 1);
        assert_eq!(tx.num_spends, 1);
        assert_eq!(tx.num_outputs, 1);
        assert_eq!(tx.shielded_spends.len(), 0);
        assert_eq!(tx.shielded_outputs.len(), 1);
        assert_eq!(tx.shielded_outputs[0].index, 0);
        assert_eq!(tx.shielded_outputs[0].account, AccountId(0));
        assert_eq!(tx.shielded_outputs[0].note.value, 5);

        // Check that the witness root matches
        assert_eq!(tx.shielded_outputs[0].witness.root(), tree.root());
    }

    #[test]
    fn scan_block_with_my_spend() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);
        let nf = Nullifier([7; 32]);
        let account = AccountId(12);

        let cb = fake_compact_block(1u32.into(), nf, extfvk, Amount::from_u64(5).unwrap(), false);
        assert_eq!(cb.vtx.len(), 2);

        let mut tree = CommitmentTree::new();
        let txs = scan_block(
            &Network::TestNetwork,
            cb,
            &[],
            &[(account, nf.clone())],
            &mut tree,
            &mut [],
        );
        assert_eq!(txs.len(), 1);

        let tx = &txs[0];
        assert_eq!(tx.index, 1);
        assert_eq!(tx.num_spends, 1);
        assert_eq!(tx.num_outputs, 1);
        assert_eq!(tx.shielded_spends.len(), 1);
        assert_eq!(tx.shielded_outputs.len(), 0);
        assert_eq!(tx.shielded_spends[0].index, 0);
        assert_eq!(tx.shielded_spends[0].nf, nf);
        assert_eq!(tx.shielded_spends[0].account, account);
    }
}
