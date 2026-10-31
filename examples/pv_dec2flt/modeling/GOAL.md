# Decimal-to-binary64 parsing

## Scope and vocabulary

The subject is the `f64` specialization of Rust's decimal parser and its
helper functions, including executions with integer overflow checks enabled.
Public results are the returned floats and errors.

A numeric literal has an optional leading sign, an ASCII decimal significand
with at least one digit and at most one decimal point, and an optional `e` or
`E` exponent with an optional sign and at least one ASCII digit. Special
literals are `inf`, `infinity`, and `nan`, matched ASCII case-insensitively,
with an optional leading sign. The whole input must match; whitespace and
other extra characters are invalid. An unsigned literal omits the leading
sign, while its exponent may still have a sign.

The exact value of a numeric literal is its ordinary mathematical decimal
value. Rounding means IEEE 754 binary64 round-to-nearest, ties-to-even,
including subnormal results, signed zero, and overflow to infinity.

For auxiliary claims, the **accumulated-exponent value** retains every
significand digit but interprets the explicit exponent using the source's
bounded exponent-accumulation rule. This is an implementation-level diagnostic
value, distinct from the exact value required by the public contract.

## parser_totality

`dec2flt`

**Totality.** For every Rust string, the function terminates normally with a
result or parsing error, without panicking.

## parser_numeric_acceptance

`dec2flt`

**Numeric acceptance.** Every numeric literal returns `Ok`.

## parser_invalid_rejection

`dec2flt`

**Invalid rejection.** Every string outside the numeric and special literal
grammar returns `Err`.

## parser_nan_acceptance

`dec2flt`

**NaN acceptance.** Every NaN literal returns `Ok` of a float satisfying
`is_nan()`.

## parser_infinity

`dec2flt`

**Infinity.** Every infinity literal returns `Ok` of the infinity with its
lexical sign.

## parser_non_nan_results

`dec2flt`

**No spurious NaNs.** Every successful result for an input other than a NaN
literal is not NaN.

## parser_numeric_sign

`dec2flt`

**Numeric sign.** Every successful numeric result has the literal's lexical
sign, including results that underflow to zero or overflow to infinity.

## parser_signed_zero

`dec2flt`

**Signed zero.** On acceptance, a numeric literal denoting zero returns
zero with its lexical sign.

## parser_exact_rounding

`dec2flt`

**Exact rounding.** On acceptance of any numeric literal, the returned float
is its exact value rounded to binary64.

## parser_selected_exponent

`dec2flt`

**Selected exponent range.** On a successful numeric parse, each raw pair
selected by the dispatcher for final float assembly has a biased exponent
in `0..=2047`.

## parser_selected_magnitude_sign

`dec2flt`

**Selected magnitude sign.** On a successful numeric parse, the assembled
word of each raw pair selected for final float assembly has a clear sign bit.

## dispatch_abstract_composition

`dec2flt`

**Abstract composition.** The lexical and conversion component contracts admit
implementations. For any components satisfying those contracts, the
dispatcher's normal results satisfy the classification, rounding, and sign
requirements. This includes the native shortcut, integer tier, many-digit
decision, fallback, sign handling, and final assembly.

## dispatch_component_compliance

`dec2flt`

**Component compliance.** The actual source components satisfy the contracts
used in abstract composition, including preservation of the input's rounding
target. The dispatcher establishes the helper preconditions at their call
sites.

## parser_accumulated_rounding

`dec2flt`

**Accumulated-value diagnostic.** Every accepted numeric literal is rounded
to its accumulated-exponent value.

## parser_equal_target_rounding

`dec2flt`

**Equal-target bridge.** On acceptance of a numeric literal whose exact
and accumulated-exponent values have the same rounding target, the result
has that target and the literal's lexical sign.

## number_helper_acceptance

`parse_number`

**Numeric acceptance.** Every nonempty byte slice forming an unsigned numeric
literal returns `Some`.

## number_helper_rejection

`parse_number`

**Invalid rejection.** Every nonempty byte slice that is not an unsigned
numeric literal returns `None`.

## number_helper_record_bounds

`parse_number`

**Record bounds.** For every nonempty byte slice, any returned record has
`negative = false` and `mantissa < 10^19`.

## number_helper_accumulated_value

`parse_number`

**Accumulated-value diagnostic.** For every unsigned numeric literal, a
returned record with `many_digits = false` represents its accumulated-exponent
value exactly as `mantissa * 10^exponent`. With `many_digits = true`, that value
lies in the half-open interval from `mantissa * 10^exponent` to
`(mantissa + 1) * 10^exponent`.

## special_helper_infinity

`parse_inf_nan`

**Infinity.** For every byte slice and either value of the separate sign
argument, an unsigned infinity keyword returns `Some` infinity with that sign.

## special_helper_nan

`parse_inf_nan`

**NaN.** For every byte slice and either value of the separate sign argument,
an unsigned NaN keyword returns `Some` of a NaN.

## special_helper_rejection

`parse_inf_nan`

**Rejection.** For every byte slice and either value of the separate sign
argument, an input other than an unsigned `inf`, `infinity`, or `nan` keyword
returns `None`.

## native_correct_or_defer

`try_fast_path`

**Native correctness or defer.** For every `Decimal` record, on normal return,
the result is either `None` or `Some` of the binary64 rounding of the exact
`mantissa * 10^exponent`, with the record's sign. A `Some` result also implies
`many_digits = false`. The contract applies to both the ordinary native path
and the disguised path that transfers powers of ten into the mantissa.

## integer_correct_or_defer

`compute_float`

**Correctness or defer.** For all `i64` exponents `q` and all `u64` mantissas
`w`, a normal result is either a pair with negative biased exponent (deferral)
or a pair whose assembled word is the nonnegative binary64 rounding of the
exact value `w * 10^q`.

## slow_accumulated_rounding

`parse_long_mantissa`

**Full-significand diagnostic.** For every unsigned numeric literal, a normal
result is a pair whose assembled word is the nonnegative binary64 rounding
of its accumulated-exponent value, retaining the full significand's
contribution to rounding.

## assembly_correspondence

`biased_fp_to_float`

**Assembly correspondence.** For every `BiasedFp`, the function returns
normally with float bits equal to the bitwise OR of its mantissa and its
biased exponent cast to `u64` using Rust semantics and shifted left by 52.
The word interpretation used in the other obligations agrees with this
operation for all field values, without a well-formedness precondition.

## classification_totality

Mathematical specification

**Classification totality.** Every finite byte string is classified as
numeric, special, or invalid.

## classification_determinism

Mathematical specification

**Classification determinism.** Every finite byte string has at most one
classification, and the parsed content of each numeric literal is unique.

## classification_significand_digit

Mathematical specification

**Significand digit.** Every numeric literal contains a significand digit.

## rounding_totality

Mathematical specification

**Rounding totality.** Every nonnegative real has a binary64 magnitude
rounding target, finite or infinite.

## rounding_determinism

Mathematical specification

**Rounding determinism.** For every nonnegative real, the binary64 magnitude
rounding target is unique.

## exponent_value_agreement

Mathematical specification

**Exponent agreement.** For every numeric literal whose accumulated explicit
exponent equals its mathematical explicit exponent, exact and
accumulated-exponent rounding agree.
