# GOAL

## Vocabulary

### The decimal grammar

A byte string is a **valid finite literal** when it consists of, in order,
with nothing before, between, or after beyond what is listed:

- an optional single leading `+` or `-` (ASCII);
- a significand: a possibly empty run of ASCII digits, optionally followed by
  a single `.` and a second possibly empty run of ASCII digits, with **at
  least one digit** in the two runs combined;
- an optional exponent part: `e` or `E`, then an optional single `+` or `-`,
  then **at least one** ASCII digit. An `e`/`E` not followed by (an optional
  sign and) a digit makes the whole input invalid, not merely terminates it.

A byte string is a **special keyword** when it is, after the same optional
single leading sign, one of `inf`, `infinity`, or `nan`, matched ASCII
case-insensitively and at exactly that length (so `Inf`, `INFINITY`, `nAn`
match; `infin` does not).

Every other byte string — including the empty string, anything containing
whitespace, a second sign, trailing bytes after a match, or a bare `.` — is
**invalid**. The three classes are mutually exclusive by construction.

The **lexical sign** of a valid finite literal or keyword is true exactly when
the optional leading sign is `-`.

### The two value readings

Write the significand's digits (both runs, in order) as one natural number
`coeff`, let `f` be the number of digits in the fractional (post-`.`) run, and
let the exponent digits (if any) be `d_1 ... d_k` with explicit sign giving a
direction `neg`.

- The **mathematical value** of the literal is
  `coeff * 10^(E - f)` where `E` is plus-or-minus the ordinary integer value
  of `d_1 ... d_k` (0 when there is no exponent part).
- The **implementation value** is the same except that `E` is replaced by the
  number the code actually accumulates: folding the exponent digits left to
  right by `acc -> 10*acc + d`, but only while `acc < 65536` — once the
  accumulator reaches 65536, every remaining exponent digit is ignored
  (`parse_scientific` in the crate; the constant is `0x10000`). The sign is
  applied after folding.

The two readings agree unless the exponent-digit accumulation saturates; the
significand is never truncated in either reading. Both readings are signless
(the lexical sign is tracked separately).

### Correct rounding

A **representable magnitude** is 0, a subnormal `m * 2^-1074` with natural
`m < 2^52`, or a normal `(2^52 + m) * 2^(p - 1075)` with natural `m < 2^52`
and integer `1 <= p <= 2046`. A representable magnitude is **even** when its
`m` is even (0 counts as even). The **overflow threshold** is
`2^1024 - 2^970`.

A nonnegative real `x` **rounds to** a target as follows: if
`x >= 2^1024 - 2^970`, its target is infinity; otherwise its target is the
representable magnitude nearest to `x`, with a tie between two equally near
magnitudes resolved in favor of the even one.

### The result pair and its word

On success the top-level entry point returns `Ok((pair, negative))`, where
the pair holds an unsigned 64-bit mantissa word and a signed 32-bit biased
exponent (the crate's fields `m` and `p_biased`), and `negative` is a `bool`.
The **assembled word** of a pair is the `u64` computed by the crate's
`biased_fp_to_word`: the bitwise OR of `m` with `p_biased` cast to `u64`
(Rust's sign-extending `as` cast, i.e. the exponent's 64-bit two's-complement
image) shifted left by 52. Note this is an OR, not a field insertion: mantissa
bits at position 52 and above merge into the exponent-field bits of the word.

A pair **encodes** a rounding target when: for a finite target `y`, the
assembled word's bit 63 is clear and interpreting the assembled word by the
standard IEEE 754 binary64 decode yields exactly the magnitude `y` (the
biased exponent lying in `0..=2047`); for the infinity target, the assembled
word is exactly the positive-infinity encoding — exponent field (bits 52-62)
all ones, fraction field (bits 0-51) zero, bit 63 clear.

## Obligations

### `dec2flt`

1. **Totality / panic-freedom.** For every input slice, the
   function returns normally — `Ok` or `Err` — and never panics; in
   particular, compiled with `overflow-checks=true`, no arithmetic overflow
   panic is reachable.

2. **End-to-end correctness on every normal return.** For every
   input slice, on every normal return the outcome matches the input's class:
   an invalid input yields `Err`; a `nan` keyword yields `Err` (this artifact has no NaN
   representation, so `nan` is recognized and rejected); an `inf`/`infinity`
   keyword yields `Ok` with a pair encoding the infinity target and the
   Boolean equal to the lexical sign; a valid finite literal yields `Ok`
   with the Boolean equal to the lexical sign and a pair encoding the target
   that the literal's IMPLEMENTATION value rounds to.

3. **Sign faithfulness (finite).** For every slice that is a valid
   finite literal and is accepted (`Ok`), the returned Boolean equals the
   lexical sign. This includes negative underflow (e.g. `-1e-999`, whose
   magnitude rounds to 0) — the sign is not lost when the magnitude is.

4. **Negative zero.** For every slice that is a valid finite
   literal with lexical sign `-` whose MATHEMATICAL value is exactly 0
   (`-0`, `-0.0`, `-0e99`, ...), on acceptance the returned Boolean is true.
   The sign of a zero result is observable and must be preserved.

5. **The infinity keywords.** For every slice that is an
   `inf`/`infinity` keyword (either lexical sign), the function returns `Ok`
   with a pair encoding the infinity target — the POSITIVE-infinity word:
   the lexical sign travels only in the Boolean — and the Boolean equal to
   the lexical sign.

6. **The `nan` keyword is rejected.** For every slice that is a
   `nan` keyword (either lexical sign), the function returns `Err`.

7. **Invalid input is rejected.** For every invalid slice, the
   function returns `Err`.

8. **Accepted exponents lie in the representable band.** For every
   input slice, every accepted pair's biased exponent field satisfies
   `0 <= p_biased <= 2047`. Nothing is claimed here about the mantissa field;
   the value obligations (2, 12, 13) constrain it through the assembled word.

9. **No NaN is ever returned — at word level.** For every input
   slice, no accepted pair's ASSEMBLED WORD is a NaN encoding: never an
   all-ones exponent field (bits 52-62) together with a nonzero fraction
   field (bits 0-51), regardless of bit 63. This is a claim about the
   assembled word, not about the raw pair fields: because the assembly is an
   OR, a pair such as `{m: 2^52 + 1, p_biased: 2046}` has an innocuous
   exponent field yet assembles to the NaN word `0x7ff0000000000001`, and is
   therefore forbidden by this obligation.

10. **Accepted results are pre-sign magnitudes.** For every input
    slice, every accepted pair's assembled word has bit 63 clear. The
    function computes the magnitude and the sign separately: the caller
    (upstream's `dec2flt`) assembles the word and only then applies the sign
    from the Boolean, so the negative-zero and negative-infinity bit patterns
    are never produced by this function — `-0.0` is accepted as the +0.0
    magnitude with Boolean true, `-inf` as the positive-infinity word with
    Boolean true.

11. **Dispatch composition.** In addition to obligation 2 itself, the
    dispatch structure must be verified as its own modular obligation:
    assuming only (i) the lexical-stage obligation 20, (ii) the fast-tier
    contract 21, and (iii) the slow-tier contract 22 as hypotheses, conclude
    obligation 2's per-return correctness. This pins that the top-level
    result is obtained from those three stage contracts by the dispatcher's
    control flow alone.

12. **Exact-value correctness where saturation is harmless.** For every
    slice that is a valid finite literal with the property that
    its MATHEMATICAL and IMPLEMENTATION values round to the same targets
    (in particular via obligation 19 whenever the exponent accumulation does
    not saturate), on acceptance the returned Boolean equals the lexical
    sign and the pair encodes the target the MATHEMATICAL value rounds to.

13. **Exact-value correctness, all finite literals.** For EVERY
    slice that is a valid finite literal, on acceptance the returned Boolean
    equals the lexical sign and the pair encodes the target the literal's
    MATHEMATICAL value rounds to — with no saturation-related side
    condition. This is stated at full strength deliberately; it is strictly
    stronger than obligation 12 exactly on literals whose exponent-digit
    accumulation saturates. If the code does not satisfy it, surface that;
    do not add the side condition.

#### Well-posedness of the authored specification

14. **Classification is total.** Every byte string is classified — valid
    finite literal (with sign and parsed content), special keyword (with
    sign and which keyword), or invalid.

15. **Classification is deterministic.** No byte string is classified two
    different ways; the parsed content (sign, significand, exponent) of a
    valid finite literal is unique.

16. **A finite literal has a digit.** Every byte string classified as a
    valid finite literal has at least one significand digit.

17. **Rounding is total.** Every nonnegative real rounds to some target —
    a representable magnitude below the overflow threshold, or infinity at
    or above it.

18. **Rounding is deterministic.** Every nonnegative real rounds to at most
    one target: the finite and infinity cases are separated exactly at the
    overflow threshold, and a distance tie between two representable
    magnitudes is resolved by evenness, which selects exactly one.

19. **Unsaturated exponents round identically.** For every valid finite
    literal whose accumulated exponent equals the ordinary integer value of
    its exponent digits (that is, the fold ignored no digit's contribution),
    the MATHEMATICAL and IMPLEMENTATION values round to the same targets.

### `parse_number`

20. **Lexical faithfulness.** For every slice:
    - If its bytes form a valid finite literal WITHOUT a leading sign, then
      `parse_number` returns `Some` of a record whose `negative` flag is
      false, whose `mantissa` is below 10^19, and which denotes the
      literal's IMPLEMENTATION value: when the record's truncation flag
      (`many_digits`) is unset, `mantissa * 10^exponent` equals the
      implementation value exactly; when it is set,
      `mantissa * 10^exponent <= implementation value <
      (mantissa + 1) * 10^exponent`. Otherwise `parse_number` returns
      `None`.
    - If its bytes form the unsigned `inf`/`infinity` keyword,
      `parse_inf_nan` returns `Some` of a pair encoding the infinity
      target; on the unsigned `nan` keyword and on every other byte string
      it returns `None` (in this artifact `nan` deliberately yields `None`
      — rejection — because there is no NaN representation).

### `compute_float`

21. **Correct or defer.** For EVERY pair of operands — all `i64` values `q`
    and all `u64` values `w`, with no precondition — whenever the function
    returns normally, the returned pair either has a NEGATIVE biased
    exponent (the tier's "defer to the slow path" sentinel; upstream's
    `fp_error`) or encodes the target that the exact rational `w * 10^q`
    rounds to. Nothing is claimed when the function does not return
    normally, and no panic-freedom is claimed for this internal tier.

### `parse_long_mantissa`

22. **Slow-path correctness at full precision.** For every slice
    whose bytes form a valid finite literal without a leading sign, whenever
    the function returns normally, the returned pair encodes the target that
    the literal's IMPLEMENTATION value rounds to. The slow path rereads
    every digit; this claim is at full significand precision and is not
    weakened by the fast stage's 19-digit truncation. Nothing is claimed on
    other inputs or about panics.

### The word assembly

23. **The assembly is the OR.** For every pair — all 2^64 mantissa values
    and all 2^32 biased-exponent values, with no precondition — the function
    returns normally and its result is exactly the assembled word as defined
    in the Vocabulary section: `m` OR (the 64-bit two's-complement image of
    `p_biased`, shifted left by 52, low 64 bits kept). The formalized
    reading of "assembled word" used by every obligation above must be
    proved to agree with this function's extracted behavior, so that the
    word the value obligations interpret is the word the crate computes.

