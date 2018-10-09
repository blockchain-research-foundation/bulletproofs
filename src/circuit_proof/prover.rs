#![allow(non_snake_case)]

use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::MultiscalarMul;
use merlin::Transcript;

use super::assignment::Assignment;
use super::{ConstraintSystem, LinearCombination, R1CSProof, Variable};

use errors::R1CSError;
use generators::{BulletproofGens, PedersenGens};
use inner_product_proof::InnerProductProof;
use transcript::TranscriptProtocol;

/// A [`ConstraintSystem`] implementation for use by the prover.
///
/// The lifecycle of a `ProverCS` is as follows.  The proving code
/// assembles openings `(v, v_blinding)` to the commitments to the
/// inputs to the constraint system, then passes them, along with
/// generators and a transcript, to [`ProverCS::new`].  This
/// initializes the `ProverCS` and returns [`Variable`]s corresponding
/// to the inputs.
///
/// The prover can then pass the `ProverCS` and the external variables
/// to gadget code to build the constraints, before finally calling
/// [`ProverCS::prove`], which consumes the `ProverCS`, synthesizes
/// the witness, and constructs the proof.
pub struct ProverCS<'a, 'b> {
    transcript: &'a mut Transcript,
    bp_gens: &'b BulletproofGens,
    pc_gens: &'b PedersenGens,
    constraints: Vec<LinearCombination>,
    a_L: Vec<Scalar>,
    a_R: Vec<Scalar>,
    a_O: Vec<Scalar>,
    v: Vec<Scalar>,
    v_blinding: Vec<Scalar>,
}

impl<'a, 'b> ConstraintSystem for ProverCS<'a, 'b> {
    fn assign_multiplier(
        &mut self,
        left: Assignment,
        right: Assignment,
        out: Assignment,
    ) -> Result<(Variable, Variable, Variable), R1CSError> {
        // Unwrap all of l,r,o up front to ensure we leave the CS in a
        // consistent state if any are missing assignments
        let l = left?;
        let r = right?;
        let o = out?;
        // Now commit to the assignment
        self.a_L.push(l);
        self.a_R.push(r);
        self.a_O.push(o);
        Ok((
            Variable::MultiplierLeft(self.a_L.len() - 1),
            Variable::MultiplierRight(self.a_R.len() - 1),
            Variable::MultiplierOutput(self.a_O.len() - 1),
        ))
    }

    fn assign_uncommitted(
        &mut self,
        val_1: Assignment,
        val_2: Assignment,
    ) -> Result<(Variable, Variable), R1CSError> {
        let val_3 = val_1 * val_2;

        let (left, right, _) = self.assign_multiplier(val_1, val_2, val_3)?;
        Ok((left, right))
    }

    fn add_constraint(&mut self, lc: LinearCombination) {
        // TODO: check that the linear combinations are valid
        // (e.g. that variables are valid, that the linear combination evals to 0 for prover, etc).
        self.constraints.push(lc);
    }

    fn challenge_scalar(&mut self, label: &'static [u8]) -> Scalar {
        self.transcript.challenge_scalar(label)
    }
}

impl<'a, 'b> ProverCS<'a, 'b> {
    /// Construct an empty constraint system with specified external
    /// input variables.
    ///
    /// # Inputs
    ///
    /// The `bp_gens` and `pc_gens` are generators for Bulletproofs
    /// and for the Pedersen commitments, respectively.  The
    /// [`BulletproofGens`] should have `gens_capacity` greater than
    /// the number of multiplication constraints that will eventually
    /// be added into the constraint system.
    ///
    /// The `transcript` parameter is a Merlin proof transcript.  The
    /// `ProverCS` holds onto the `&mut Transcript` until it consumes
    /// itself during [`ProverCS::prove`], releasing its borrow of the
    /// transcript.  This ensures that the transcript cannot be
    /// altered except by the `ProverCS` before proving is complete.
    ///
    /// The `v` and `v_blinding` parameters are openings to the
    /// commitments to the external variables for the constraint
    /// system.  Passing the opening (the value together with the
    /// blinding factor) makes it possible to reference pre-existing
    /// commitments in the constraint system.  All external variables
    /// must be passed up-front, so that challenges produced by
    /// [`ConstraintSystem::challenge_scalar`] are bound to the
    /// external variables.
    ///
    /// # Returns
    ///
    /// Returns a tuple `(cs, vars, commitments)`.
    ///
    /// The first element is the newly constructed constraint system.
    ///
    /// The second element is a list of [`Variable`]s corresponding to
    /// the external inputs, which can be used to form constraints.
    ///
    /// The third element is a list of the Pedersen commitments to the
    /// external inputs, returned for convenience.
    pub fn new(
        bp_gens: &'b BulletproofGens,
        pc_gens: &'b PedersenGens,
        transcript: &'a mut Transcript,
        v: Vec<Scalar>,
        v_blinding: Vec<Scalar>,
    ) -> (Self, Vec<Variable>, Vec<CompressedRistretto>) {
        // Check that the input lengths are consistent
        assert_eq!(v.len(), v_blinding.len());
        let m = v.len();
        transcript.r1cs_domain_sep(m as u64);

        let mut variables = Vec::with_capacity(m);
        let mut commitments = Vec::with_capacity(m);

        for i in 0..m {
            // Generate pedersen commitment and commit it to the transcript
            let V = pc_gens.commit(v[i], v_blinding[i]).compress();
            transcript.commit_point(b"V", &V);
            commitments.push(V);

            // Allocate and return a variable for v_i
            variables.push(Variable::Committed(i));
        }

        let cs = ProverCS {
            pc_gens,
            bp_gens,
            transcript,
            v,
            v_blinding,
            constraints: vec![],
            a_L: vec![],
            a_R: vec![],
            a_O: vec![],
        };

        (cs, variables, commitments)
    }

    /// Use a challenge, `z`, to flatten the constraints in the
    /// constraint system into vectors used for proving and
    /// verification.
    ///
    /// # Output
    ///
    /// Returns a tuple of
    /// ```text
    /// (wL, wR, wO, wV)
    /// ```
    /// where `w{L,R,O}` is \\( z \cdot z^Q \cdot W_{L,R,O} \\).
    fn flattened_constraints(
        &mut self,
        z: &Scalar,
    ) -> (Vec<Scalar>, Vec<Scalar>, Vec<Scalar>, Vec<Scalar>) {
        let n = self.a_L.len();
        let m = self.v.len();

        let mut wL = vec![Scalar::zero(); n];
        let mut wR = vec![Scalar::zero(); n];
        let mut wO = vec![Scalar::zero(); n];
        let mut wV = vec![Scalar::zero(); m];

        let mut exp_z = *z;
        for lc in self.constraints.iter() {
            for (var, coeff) in &lc.terms {
                match var {
                    Variable::MultiplierLeft(i) => {
                        wL[*i] += exp_z * coeff;
                    }
                    Variable::MultiplierRight(i) => {
                        wR[*i] += exp_z * coeff;
                    }
                    Variable::MultiplierOutput(i) => {
                        wO[*i] += exp_z * coeff;
                    }
                    Variable::Committed(i) => {
                        wV[*i] -= exp_z * coeff;
                    }
                    Variable::One() => {
                        // The prover doesn't need to handle constant terms
                    }
                }
            }
            exp_z *= z;
        }

        (wL, wR, wO, wV)
    }

    /// Consume this `ConstraintSystem` to produce a proof.
    pub fn prove(mut self) -> Result<R1CSProof, R1CSError> {
        use std::iter;
        use util;

        // 0. Pad zeros to the next power of two (or do that implicitly when creating vectors)

        // If the number of multiplications is not 0 or a power of 2, then pad the circuit.
        let temp_n = self.a_L.len();
        if !(temp_n == 0 || temp_n.is_power_of_two()) {
            let pad = temp_n.next_power_of_two() - temp_n;
            for _ in 0..pad {
                let _ = self.assign_multiplier(
                    Scalar::zero().into(),
                    Scalar::zero().into(),
                    Scalar::zero().into(),
                );
            }
        }
        let n = self.a_L.len();
        if self.bp_gens.gens_capacity < n {
            return Err(R1CSError::InvalidGeneratorsLength);
        }

        // We are performing a single-party circuit proof, so party index is 0.
        let gens = self.bp_gens.share(0);

        // 1. Create a `TranscriptRng` from the high-level witness data

        let mut rng = {
            let mut builder = self.transcript.build_rng();

            // Commit the blinding factors for the input wires
            for v_b in &self.v_blinding {
                builder = builder.commit_witness_bytes(b"v_blinding", v_b.as_bytes());
            }

            use rand::thread_rng;
            builder.finalize(&mut thread_rng())
        };

        // 3. Choose blinding factors and form commitments to low-level witness data

        let i_blinding = Scalar::random(&mut rng);
        let o_blinding = Scalar::random(&mut rng);
        let s_blinding = Scalar::random(&mut rng);

        let s_L: Vec<Scalar> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let s_R: Vec<Scalar> = (0..n).map(|_| Scalar::random(&mut rng)).collect();

        // A_I = <a_L, G> + <a_R, H> + i_blinding * B_blinding
        let A_I = RistrettoPoint::multiscalar_mul(
            iter::once(&i_blinding)
                .chain(self.a_L.iter())
                .chain(self.a_R.iter()),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n))
                .chain(gens.H(n)),
        )
        .compress();

        // A_O = <a_O, G> + o_blinding * B_blinding
        let A_O = RistrettoPoint::multiscalar_mul(
            iter::once(&o_blinding).chain(self.a_O.iter()),
            iter::once(&self.pc_gens.B_blinding).chain(gens.G(n)),
        )
        .compress();

        // S = <s_L, G> + <s_R, H> + s_blinding * B_blinding
        let S = RistrettoPoint::multiscalar_mul(
            iter::once(&s_blinding).chain(s_L.iter()).chain(s_R.iter()),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n))
                .chain(gens.H(n)),
        )
        .compress();

        self.transcript.commit_point(b"A_I", &A_I);
        self.transcript.commit_point(b"A_O", &A_O);
        self.transcript.commit_point(b"S", &S);

        // 4. Compute blinded vector polynomials l(x) and r(x)

        let y = self.transcript.challenge_scalar(b"y");
        let z = self.transcript.challenge_scalar(b"z");

        let (wL, wR, wO, wV) = self.flattened_constraints(&z);

        let mut l_poly = util::VecPoly3::zero(n);
        let mut r_poly = util::VecPoly3::zero(n);

        let mut exp_y = Scalar::one(); // y^n starting at n=0
        let y_inv = y.invert();
        let exp_y_inv = util::exp_iter(y_inv).take(n).collect::<Vec<_>>();

        for i in 0..n {
            // l_poly.0 = 0
            // l_poly.1 = a_L + y^-n * (z * z^Q * W_R)
            l_poly.1[i] = self.a_L[i] + exp_y_inv[i] * wR[i];
            // l_poly.2 = a_O
            l_poly.2[i] = self.a_O[i];
            // l_poly.3 = s_L
            l_poly.3[i] = s_L[i];
            // r_poly.0 = (z * z^Q * W_O) - y^n
            r_poly.0[i] = wO[i] - exp_y;
            // r_poly.1 = y^n * a_R + (z * z^Q * W_L)
            r_poly.1[i] = exp_y * self.a_R[i] + wL[i];
            // r_poly.2 = 0
            // r_poly.3 = y^n * s_R
            r_poly.3[i] = exp_y * s_R[i];

            exp_y = exp_y * y; // y^i -> y^(i+1)
        }

        let t_poly = l_poly.inner_product(&r_poly);

        let t_1_blinding = Scalar::random(&mut rng);
        let t_3_blinding = Scalar::random(&mut rng);
        let t_4_blinding = Scalar::random(&mut rng);
        let t_5_blinding = Scalar::random(&mut rng);
        let t_6_blinding = Scalar::random(&mut rng);

        let T_1 = self.pc_gens.commit(t_poly.t1, t_1_blinding).compress();
        let T_3 = self.pc_gens.commit(t_poly.t3, t_3_blinding).compress();
        let T_4 = self.pc_gens.commit(t_poly.t4, t_4_blinding).compress();
        let T_5 = self.pc_gens.commit(t_poly.t5, t_5_blinding).compress();
        let T_6 = self.pc_gens.commit(t_poly.t6, t_6_blinding).compress();

        self.transcript.commit_point(b"T_1", &T_1);
        self.transcript.commit_point(b"T_3", &T_3);
        self.transcript.commit_point(b"T_4", &T_4);
        self.transcript.commit_point(b"T_5", &T_5);
        self.transcript.commit_point(b"T_6", &T_6);

        let x = self.transcript.challenge_scalar(b"x");

        // t_2_blinding = <z*z^Q, W_V * v_blinding>
        // in the t_x_blinding calculations, line 76.
        let t_2_blinding = wV
            .iter()
            .zip(self.v_blinding.iter())
            .map(|(c, v_blinding)| c * v_blinding)
            .sum();

        let t_blinding_poly = util::Poly6 {
            t1: t_1_blinding,
            t2: t_2_blinding,
            t3: t_3_blinding,
            t4: t_4_blinding,
            t5: t_5_blinding,
            t6: t_6_blinding,
        };

        let t_x = t_poly.eval(x);
        let t_x_blinding = t_blinding_poly.eval(x);
        let l_vec = l_poly.eval(x);
        let r_vec = r_poly.eval(x);
        let e_blinding = x * (i_blinding + x * (o_blinding + x * s_blinding));

        self.transcript.commit_scalar(b"t_x", &t_x);
        self.transcript
            .commit_scalar(b"t_x_blinding", &t_x_blinding);
        self.transcript.commit_scalar(b"e_blinding", &e_blinding);

        // Get a challenge value to combine statements for the IPP
        let w = self.transcript.challenge_scalar(b"w");
        let Q = w * self.pc_gens.B;

        let ipp_proof = InnerProductProof::create(
            self.transcript,
            &Q,
            &exp_y_inv,
            gens.G(n).cloned().collect(),
            gens.H(n).cloned().collect(),
            l_vec,
            r_vec,
        );

        Ok(R1CSProof {
            A_I,
            A_O,
            S,
            T_1,
            T_3,
            T_4,
            T_5,
            T_6,
            t_x,
            t_x_blinding,
            e_blinding,
            ipp_proof,
        })
    }
}
