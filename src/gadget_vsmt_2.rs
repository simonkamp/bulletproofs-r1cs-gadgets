extern crate rand;
extern crate curve25519_dalek;
extern crate merlin;
extern crate bulletproofs;

use std::collections::HashMap;
use rand::SeedableRng;
use rand::rngs::OsRng;
use curve25519_dalek::scalar::Scalar;
use bulletproofs::r1cs::{ConstraintSystem, R1CSError, R1CSProof, Variable, Prover, Verifier};
use bulletproofs::{BulletproofGens, PedersenGens};
use merlin::Transcript;
use bulletproofs::r1cs::LinearCombination;

use crate::scalar_utils::{ScalarBytes, ScalarBits, get_bits};
use crate::r1cs_utils::{AllocatedScalar, constrain_lc_with_scalar};
// use crate::gadget_mimc::{mimc, MIMC_ROUNDS, mimc_hash_2, mimc_gadget};
use crate::gadget_poseidon::{PoseidonParams, Poseidon_hash_2, Poseidon_hash_2_constraints, Poseidon_hash_2_gadget, SboxType,
                             allocate_statics_for_prover, allocate_statics_for_verifier};

type DBVal = (Scalar, Scalar);

pub const TreeDepth: usize = 32;

// TODO: ABSTRACT HASH FUNCTION BETTER

pub struct VanillaSparseMerkleTree<'a> {
    pub depth: usize,
    empty_tree_hashes: Vec<Scalar>,
    db: HashMap<ScalarBytes, DBVal>,
    //hash_constants: &'a [Scalar],
    hash_params: &'a PoseidonParams,
    pub root: Scalar
}

impl<'a> VanillaSparseMerkleTree<'a> {
    pub fn new(hash_params: &'a PoseidonParams) -> VanillaSparseMerkleTree<'a> {
        let depth = TreeDepth;
        let mut db = HashMap::new();
        let mut empty_tree_hashes: Vec<Scalar> = vec![];
        empty_tree_hashes.push(Scalar::zero());
        for i in 1..=depth {
            let prev = empty_tree_hashes[i-1];
            //let new = mimc(&prev, &prev, hash_constants);
            let new = Poseidon_hash_2(prev.clone(), prev.clone(), hash_params, &SboxType::Inverse);
            let key = new.to_bytes();

            db.insert(key, (prev, prev));
            empty_tree_hashes.push(new);
        }

        let root = empty_tree_hashes[depth].clone();

        VanillaSparseMerkleTree {
            depth,
            empty_tree_hashes,
            db,
            hash_params,
            root
        }
    }

    pub fn update(&mut self, idx: Scalar, val: Scalar) -> Scalar {

        // Find path to insert the new key
        let mut sidenodes_wrap = Some(Vec::<Scalar>::new());
        self.get(idx, &mut sidenodes_wrap);
        let mut sidenodes: Vec<Scalar> = sidenodes_wrap.unwrap();

        let mut cur_idx = ScalarBits::from_scalar(&idx, TreeDepth);
        let mut cur_val = val.clone();

        for i in 0..self.depth {
            let side_elem = sidenodes.pop().unwrap();
            let new_val = {
                if cur_idx.is_lsb_set() {
                    // LSB is set, so put new value on right
                    //let h =  mimc(&side_elem, &cur_val, self.hash_constants);
                    let h =  Poseidon_hash_2(side_elem.clone(), cur_val.clone(), self.hash_params, &SboxType::Inverse);
                    self.update_db_with_key_val(h, (side_elem, cur_val));
                    h
                } else {
                    // LSB is unset, so put new value on left
                    //let h =  mimc(&cur_val, &side_elem, self.hash_constants);
                    let h =  Poseidon_hash_2(cur_val.clone(), side_elem.clone(), self.hash_params, &SboxType::Inverse);
                    self.update_db_with_key_val(h, (cur_val, side_elem));
                    h
                }
            };
            //println!("Root at level {} is {:?}", i, &cur_val);
            cur_idx.shr();
            cur_val = new_val;
        }

        self.root = cur_val;

        cur_val
    }

    /// Get a value from tree, if `proof` is not None, populate `proof` with the merkle proof
    pub fn get(&self, idx: Scalar, proof: &mut Option<Vec<Scalar>>) -> Scalar {
        let mut cur_idx = ScalarBits::from_scalar(&idx, TreeDepth);
        let mut cur_node = self.root.clone();

        let need_proof = proof.is_some();
        let mut proof_vec = Vec::<Scalar>::new();

        for i in 0..self.depth {
            let k = cur_node.to_bytes();
            let v = self.db.get(&k).unwrap();
            if cur_idx.is_msb_set() {
                // MSB is set, traverse to right subtree
                cur_node = v.1;
                if need_proof { proof_vec.push(v.0); }
            } else {
                // MSB is unset, traverse to left subtree
                cur_node = v.0;
                if need_proof { proof_vec.push(v.1); }
            }
            cur_idx.shl();
        }

        match proof {
            Some(v) => {
                v.extend_from_slice(&proof_vec);
            }
            None => ()
        }

        cur_node
    }

    /// Verify a merkle proof, if `root` is None, use the current root else use given root
    pub fn verify_proof(&self, idx: Scalar, val: Scalar, proof: &[Scalar], root: Option<&Scalar>) -> bool {
        let mut cur_idx = ScalarBits::from_scalar(&idx, TreeDepth);
        let mut cur_val = val.clone();

        for i in 0..self.depth {
            cur_val = {
                if cur_idx.is_lsb_set() {
                    // mimc(&proof[self.depth-1-i], &cur_val, self.hash_constants)
                    Poseidon_hash_2(proof[self.depth-1-i].clone(), cur_val.clone(), self.hash_params, &SboxType::Inverse)
                } else {
                    // mimc(&cur_val, &proof[self.depth-1-i], self.hash_constants)
                    Poseidon_hash_2(cur_val.clone(), proof[self.depth-1-i].clone(), self.hash_params, &SboxType::Inverse)
                }
            };

            cur_idx.shr();
        }

        // Check if root is equal to cur_val
        match root {
            Some(r) => {
                cur_val == *r
            }
            None => {
                cur_val == self.root
            }
        }
    }

    fn update_db_with_key_val(&mut self, key: Scalar, val: DBVal) {
        self.db.insert(key.to_bytes(), val);
    }
}


/// left = (1-leaf_side) * leaf + (leaf_side * proof_node)
/// right = leaf_side * leaf + ((1-leaf_side) * proof_node))
pub fn vanilla_merkle_merkle_tree_verif_gadget<CS: ConstraintSystem>(
    cs: &mut CS,
    depth: usize,
    root: &Scalar,
    leaf_val: AllocatedScalar,
    leaf_index_bits: Vec<AllocatedScalar>,
    proof_nodes: Vec<AllocatedScalar>,
    statics: Vec<AllocatedScalar>,
    poseidon_params: &PoseidonParams
) -> Result<(), R1CSError> {

    let mut prev_hash = LinearCombination::default();

    let statics: Vec<LinearCombination> = statics.iter().map(|s| s.variable.into()).collect();

    for i in 0..depth {
        let leaf_val_lc = if i == 0 {
            LinearCombination::from(leaf_val.variable)
        } else {
            prev_hash.clone()
        };
        let one_minus_leaf_side: LinearCombination = Variable::One() - leaf_index_bits[i].variable;

        let (_, _, left_1) = cs.multiply(one_minus_leaf_side.clone(), leaf_val_lc.clone());
        let (_, _, left_2) = cs.multiply(leaf_index_bits[i].variable.into(), proof_nodes[i].variable.into());
        let left = left_1 + left_2;

        let (_, _, right_1) = cs.multiply(leaf_index_bits[i].variable.into(), leaf_val_lc);
        let (_, _, right_2) = cs.multiply(one_minus_leaf_side, proof_nodes[i].variable.into());
        let right = right_1 + right_2;

        // prev_hash = mimc_hash_2::<CS>(cs, left, right, mimc_rounds, mimc_constants)?;
        prev_hash = Poseidon_hash_2_constraints::<CS>(cs, left, right, statics.clone(), poseidon_params, &SboxType::Inverse)?;
    }

    constrain_lc_with_scalar::<CS>(cs, prev_hash, root);

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use merlin::Transcript;
    use curve25519_dalek::constants::BASEPOINT_ORDER;
    use rand::SeedableRng;
    use super::rand::rngs::StdRng;
    // For benchmarking
    use std::time::{Duration, Instant};

    #[test]
    fn test_vanilla_sparse_merkle_tree() {
        let mut test_rng: OsRng = OsRng::default();

        // Generate the MiMC round constants
        /*let constants = (0..MIMC_ROUNDS).map(|_| Scalar::random(&mut test_rng)).collect::<Vec<_>>();
        let mut tree = VanillaSparseMerkleTree::new(&constants);*/
        let width = 6;
        let (full_b, full_e) = (4, 4);
        let partial_rounds = 140;
        let p_params = PoseidonParams::new(width, full_b, full_e, partial_rounds);
        let mut tree = VanillaSparseMerkleTree::new(&p_params);

        for i in 1..10 {
            let s = Scalar::from(i as u32);
            tree.update(s, s);
        }

        for i in 1..10 {
            let s = Scalar::from(i as u32);
            assert_eq!(s, tree.get(s, &mut None));
            let mut proof_vec = Vec::<Scalar>::new();
            let mut proof = Some(proof_vec);
            assert_eq!(s, tree.get(s, &mut proof));
            proof_vec = proof.unwrap();
            assert!(tree.verify_proof(s, s, &proof_vec, None));
            assert!(tree.verify_proof(s, s, &proof_vec, Some(&tree.root)));
        }

        let kvs: Vec<(Scalar, Scalar)> = (0..100).map(|_| (Scalar::random(&mut test_rng), Scalar::random(&mut test_rng))).collect();
        for i in 0..kvs.len() {
            tree.update(kvs[i].0, kvs[i].1);
        }

        for i in 0..kvs.len() {
            assert_eq!(kvs[i].1, tree.get(kvs[i].0, &mut None));
        }
    }

    #[test]
    fn test_VSMT_Verif() {
        let mut test_rng: StdRng = SeedableRng::from_seed([24u8; 32]);

        // Generate the MiMC round constants
        /*let constants = (0..MIMC_ROUNDS).map(|_| Scalar::random(&mut test_rng)).collect::<Vec<_>>();
        let mut tree = VanillaSparseMerkleTree::new(&constants);*/

        let width = 6;
        let (full_b, full_e) = (8, 8);
        let partial_rounds = 105;
        let total_rounds = full_b + partial_rounds + full_e;
        let p_params = PoseidonParams::new(width, full_b, full_e, partial_rounds);
        let mut tree = VanillaSparseMerkleTree::new(&p_params);

        for i in 1..=10 {
            let s = Scalar::from(i as u32);
            tree.update(s, s);
        }

        let mut merkle_proof_vec = Vec::<Scalar>::new();
        let mut merkle_proof = Some(merkle_proof_vec);
        let k =  Scalar::from(7u32);
        assert_eq!(k, tree.get(k, &mut merkle_proof));
        merkle_proof_vec = merkle_proof.unwrap();
        assert!(tree.verify_proof(k, k, &merkle_proof_vec, None));
        assert!(tree.verify_proof(k, k, &merkle_proof_vec, Some(&tree.root)));

        let pc_gens = PedersenGens::default();
        let gens_capacity = 1 << 15; // 2^15 is minimal
        let bp_gens = BulletproofGens::new(gens_capacity, 1);

        let (proof, commitments) = {
            let mut prover_transcript = Transcript::new(b"VSMT");
            let mut prover = Prover::new(&pc_gens, &mut prover_transcript);

            let (com_leaf, var_leaf) = prover.commit(k, Scalar::random(&mut test_rng));
            let leaf_alloc_scalar = AllocatedScalar {
                variable: var_leaf,
                assignment: Some(k),
            };

            let mut leaf_index_comms = vec![];
            let mut leaf_index_vars = vec![];
            let mut leaf_index_alloc_scalars = vec![];
            for b in get_bits(&k, TreeDepth).iter().take(tree.depth) {
                let val: Scalar = Scalar::from(*b as u8);
                let (c, v) = prover.commit(val.clone(), Scalar::random(&mut test_rng));
                leaf_index_comms.push(c);
                leaf_index_vars.push(v);
                leaf_index_alloc_scalars.push(AllocatedScalar {
                    variable: v,
                    assignment: Some(val),
                });
            }

            let mut proof_comms = vec![];
            let mut proof_vars = vec![];
            let mut proof_alloc_scalars = vec![];
            for p in merkle_proof_vec.iter().rev() {
                let (c, v) = prover.commit(*p, Scalar::random(&mut test_rng));
                proof_comms.push(c);
                proof_vars.push(v);
                proof_alloc_scalars.push(AllocatedScalar {
                    variable: v,
                    assignment: Some(*p),
                });
            }

            let num_statics = 4;
            let statics = allocate_statics_for_prover(&mut prover, num_statics);

            let start = Instant::now();
            assert!(vanilla_merkle_merkle_tree_verif_gadget(
                &mut prover,
                tree.depth,
                &tree.root,
                leaf_alloc_scalar,
                leaf_index_alloc_scalars,
                proof_alloc_scalars,
                statics,
                &p_params).is_ok());

//            println!("For tree height {} and MiMC rounds {}, no of constraints is {}", tree.depth, &MIMC_ROUNDS, &prover.num_constraints());

            println!("For binary tree of height {} and Poseidon rounds {}, no of multipliers is {} and constraints is {}", tree.depth, total_rounds, &prover.num_multipliers(), &prover.num_constraints());

            let proof = prover.prove(&bp_gens).unwrap();
            let end = start.elapsed();

            println!("Proving time is {:?}", end);

            (proof, (com_leaf, leaf_index_comms, proof_comms))
        };

        let mut verifier_transcript = Transcript::new(b"VSMT");
        let mut verifier = Verifier::new(&mut verifier_transcript);
        let var_leaf = verifier.commit(commitments.0);
        let leaf_alloc_scalar = AllocatedScalar {
            variable: var_leaf,
            assignment: None,
        };

        let mut leaf_index_alloc_scalars = vec![];
        for l in commitments.1 {
            let v = verifier.commit(l);
            leaf_index_alloc_scalars.push(AllocatedScalar {
                variable: v,
                assignment: None,
            });
        }

        let mut proof_alloc_scalars = vec![];
        for p in commitments.2 {
            let v = verifier.commit(p);
            proof_alloc_scalars.push(AllocatedScalar {
                variable: v,
                assignment: None,
            });
        }

        let num_statics = 4;
        let statics = allocate_statics_for_verifier(&mut verifier, num_statics, &pc_gens);

        let start = Instant::now();
        assert!(vanilla_merkle_merkle_tree_verif_gadget(
            &mut verifier,
            tree.depth,
            &tree.root,
            leaf_alloc_scalar,
            leaf_index_alloc_scalars,
            proof_alloc_scalars,
            statics,
            &p_params).is_ok());

        assert!(verifier.verify(&proof, &pc_gens, &bp_gens).is_ok());
        let end = start.elapsed();

        println!("Verification time is {:?}", end);
    }
}