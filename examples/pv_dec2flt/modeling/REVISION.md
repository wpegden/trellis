# Decimal parser goal revision

This note records the migration from the original 23-goal campaign to the
requirements in [GOAL.json](GOAL.json), rendered in [GOAL.md](GOAL.md). It is review and migration material, not
part of the uploaded goal specification. No existing proof status is carried
over by this document.

The revised specification uses the actual `f64` entry and helper interfaces,
separately tracks public behaviors, and retains the original mathematical and
implementation-level diagnostic questions. The source program is unchanged.

## Original clause coverage

| Original goal | Revised obligation or disposition |
|---|---|
| 01 — totality | `dec2flt` / Totality. Covers every Rust string and overflow-checked execution. |
| 02 — bundled end-to-end behavior | Split among Numeric acceptance, Invalid rejection, NaN acceptance, Infinity, Numeric sign, and Accumulated-value diagnostic. Public Exact rounding uses the mathematical input value. |
| 03 — finite sign | `dec2flt` / Numeric sign, including underflow and overflow. |
| 04 — negative zero | `dec2flt` / Signed zero, with both lexical signs explicit. Numeric sign also covers underflowed zero. |
| 05 — infinity keywords | `dec2flt` / Infinity. |
| 06 — reject NaN | Replaced by `dec2flt` / NaN acceptance, matching the intended parser contract. |
| 07 — invalid rejection | `dec2flt` / Invalid rejection. Includes empty, malformed, whitespace-containing, and non-ASCII Rust strings. |
| 08 — exponent range | `dec2flt` / Selected exponent range, on genuine integer-tier pairs selected for assembly. |
| 09 — prohibit every NaN | Repaired to `dec2flt` / No spurious NaNs; documented NaN inputs are permitted. |
| 10 — positive raw magnitude | `dec2flt` / Selected magnitude sign. Applied to genuine integer-tier outputs; adapter-only output fields and public NaN representation guarantees are retired. |
| 11 — composition | `dispatch_abstract_composition` and `dispatch_component_compliance` are separate targets. Abstract contracts must admit implementations; concrete compliance includes preservation of rounding targets and caller preconditions. |
| 12 — equal-rounding bridge | `dec2flt` / Equal-target bridge. |
| 13 — full mathematical correctness | `dec2flt` / Exact rounding. Preserved without an exponent- or length-related exception. |
| 14 — total classification | Specification / Classification totality. |
| 15 — deterministic classification | Specification / Classification determinism, including parsed content. |
| 16 — significand digit | Specification / Significand digit. |
| 17 — total rounding | Specification / Rounding totality over all nonnegative reals. |
| 18 — deterministic rounding | Specification / Rounding determinism. |
| 19 — exponent agreement | Specification / Exponent agreement. |
| 20 — bundled lexical behavior | `parse_number` has separate acceptance, rejection, record-bound, and accumulated-value obligations on nonempty inputs. `parse_inf_nan` has separate infinity, NaN, and rejection obligations. Dispatcher composition must establish helper preconditions. |
| 21 — integer correct-or-defer | `compute_float` / Correctness or defer, retaining all `i64`/`u64` operands and normal-return conditioning. |
| 22 — full-significand slow tier | `parse_long_mantissa` / Full-significand diagnostic, retaining every unsigned numeric literal and normal-return conditioning. |
| 23 — all-fields OR assembly | `biased_fp_to_float` / Assembly correspondence, retaining all field values and using the actual source helper. |

The added tracked native contract is `try_fast_path` / Native correctness or
defer. It covers both ordinary and disguised native conversion. Numeric
acceptance and the corrected NaN requirements are independently tracked
repairs of existing public behavior, rather than new subsystem scope.

## Contract evidence and domains

The module's [problem statement](../source/target/dec2flt/mod.rs) specifies
rounding of the exact decimal value and describes panics as bugs. It also
describes sign processing and the progression through conversion tiers.
The public grammar is restated in GOAL so the input has a self-contained
lexical contract. Its source authority is Rust's `FromStr` float contract;
the local audit checked the corresponding `core/src/num/float_parse.rs`
documentation in the installed source distribution.

[parse.rs](../source/target/dec2flt/parse.rs) supplies the numeric helper's
nonempty-input assertion, the caller-visible record fields, and special-value
classification. The [dispatcher](../source/target/dec2flt/mod.rs) guards empty
input and a lone sign before calling the numeric helper. This justifies the
helper domain without restricting public totality.

[decimal.rs](../source/target/dec2flt/decimal.rs) describes ordinary and
disguised native conversion. Its early eligibility check rejects records
with truncated digits. [lemire.rs](../source/target/dec2flt/lemire.rs) describes
correct conversion or deferral over its operand types.
[slow.rs](../source/target/dec2flt/slow.rs) describes full-precision fallback
rounding. Their contracts remain distinct from successful public conversion.

Accumulated-exponent claims characterize an internal approximation. The
source's explicit-exponent scan is identified in setup and given a reviewed
mathematical definition. Such claims are diagnostic; proving them does not
establish preservation of the original literal's rounding target. Concrete
component compliance for public composition must establish the latter; it
does not require exact-value equality when an approximation preserves rounding.

Exact source/model correspondence remains a setup and review requirement,
including distinctions observable in the selected program's float words.
The public NaN acceptance requirement itself only specifies `is_nan()`.

## Intake and reuse

The native input contains 32 independently identified statements: 15 about
the entry and its composition, four numeric-parser contracts, three special-
parser contracts, four other helper contracts, and six mathematical
specification properties. Each ID is an actual native verification target.
The file is not an input to the callable-contract upload launcher.

The rendered GOAL.md is generated with the same context/section concatenation
as `trellis/modeling/driver.py:initialize`. Native export in
`trellis/modeling/native.py` assigns the retained IDs directly to
`pv_tablet.verification_targets`. A preflight should compare the commissioned
ID inventory with that registered set and account for any explicit declines.
No tracking or scheduling instructions are part of GOAL.

The original campaign remains the record for its original statements and
results. Its source/model evidence and supporting proofs are candidates for
checked reuse. Revalidate bindings, definitions, assumptions, and dependencies
in the new campaign. Changed goals need fresh correspondence review; the old
composition proof by contradiction does not satisfy the revised modular goal.

Known saturation and checked-overflow counterexamples remain relevant to the
strong public claims. They are evidence to reassess under the new statements,
not grounds to narrow those statements or transfer an old completion status.
