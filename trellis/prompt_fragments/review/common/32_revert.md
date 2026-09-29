## Revert options

Two revert levers are available when the review contract allows them.

### `reset = last_commit`
If the review contract allows `reset = last_commit`, you may use it to discard unaccepted live changes and resume from the last accepted checkpoint (one checkpoint back).

Use `last_commit` when the current live state is a bad direction and the right next step is to back up before continuing. This reset is kernel-handled; it does not change blocker actions or expand worker legality on its own.
