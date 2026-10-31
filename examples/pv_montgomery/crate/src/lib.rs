//! Candidate PV showpiece targets — post-quantum-crypto arithmetic kernels.
//! Safe Rust only; extracted via Charon/Aeneas to Lean and proved against the
//! generated model. These are MEATIER than the Phase-0b `barrett_reduce` warm-up.

// ====================================================================
// ML-KEM (Kyber) constants — q = 3329.
// ====================================================================
const FIELD_MODULUS: i32 = 3329;

// Montgomery: R = 2^16 mod q, used because limbs are i16/i32.
// MONTGOMERY_SHIFT = 16; MONTGOMERY_R = 2^16.
const MONTGOMERY_SHIFT: u32 = 16;

// Barrett (kept for reference / comparison with the spike).
const BARRETT_SHIFT: i64 = 26;
const BARRETT_R: i64 = 1 << BARRETT_SHIFT;
const BARRETT_MULTIPLIER: i64 = 20159;

/// Barrett reduction (the Phase-0b headline; congruence already proved).
/// Kept here so the new targets share one crate.
pub fn barrett_reduce(input: i32) -> i32 {
    let t = ((input as i64) * BARRETT_MULTIPLIER) + (BARRETT_R >> 1);
    let quotient = (t >> BARRETT_SHIFT) as i32;
    let remainder = input - (quotient * FIELD_MODULUS);
    remainder
}

/// Montgomery reduction as shipped in libcrux ML-KEM (the real kernel).
/// Given `value` with `|value| < q * 2^15`, returns a representative of
/// `value * R^{-1} (mod q)` in the range (-q, q), where R = 2^16.
///
/// The libcrux implementation:
///   let k = (value as i16 as i32) * (QINV as i32);          // low 16 bits * q^{-1} mod 2^16
///   let k_times_modulus = (k as i16 as i32) * (q as i32);
///   let c = k_times_modulus >> MONTGOMERY_SHIFT;
///   let value_high = value >> MONTGOMERY_SHIFT;
///   value_high - c
/// where QINV = -3327 = q^{-1} mod 2^16 (since q*QINV ≡ -1, the signed form).
///
/// Correctness: result ≡ value * R^{-1} (mod q), i.e. result * R ≡ value (mod q).
/// We use the `i16 as i32` truncation to mod-2^16 the low limb.
const MONTGOMERY_QINV: i32 = -3327; // q^{-1} mod 2^16 in signed form (montgomery)

pub fn montgomery_reduce(value: i32) -> i32 {
    // truncate to the low 16 bits (signed), the `value as i16 as i32` idiom
    let k = (value as i16 as i32).wrapping_mul(MONTGOMERY_QINV);
    let k_times_modulus = (k as i16 as i32) * FIELD_MODULUS;
    let c = k_times_modulus >> MONTGOMERY_SHIFT;
    let value_high = value >> MONTGOMERY_SHIFT;
    value_high - c
}

/// Montgomery multiplication: (a * b) * R^{-1} mod q, the field multiply in the
/// Montgomery domain used pervasively in the NTT.
/// Correctness: result ≡ a * b * R^{-1} (mod q).
pub fn montgomery_multiply_reduce(a: i32, b: i32) -> i32 {
    montgomery_reduce(a * b)
}

/// A single NTT butterfly (Cooley-Tukey) over the Montgomery domain, as it
/// appears in the libcrux ML-KEM NTT layer. `a`, `b` are coefficients, `zeta`
/// the twiddle factor (already in Montgomery form).
///   let t = montgomery_multiply_reduce(b, zeta);
///   (a + t, a - t)   -- returned as a 2-tuple
/// Correctness: the pair is the CT butterfly of (a,b) with twiddle zeta,
/// each reduced; modular-evaluation property of one butterfly step.
pub fn ntt_butterfly(a: i32, b: i32, zeta: i32) -> (i32, i32) {
    let t = montgomery_multiply_reduce(b, zeta);
    let x = a + t;
    let y = a - t;
    (x, y)
}
