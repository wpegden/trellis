# GOAL

## Obligations

### `add_offset`

For every `(value : Int)` and `(offset : Int)`, the result agrees with the
source function's specified bounded addition behavior.

### `select_larger`

For every `(left : Nat)` and `(right : Nat)`, the result is one of the inputs
and is not smaller than either input.

### `mutate`

The function mutates its input slice as described by the source.
