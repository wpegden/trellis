# Target faithfulness

A supplied target *T* is **faithfully represented** in the tablet iff its *covering set* (the collection of tablet nodes whose `.tex` statements collectively represent *T*) expresses the full mathematical content of *T* as its reference text states it.

Concretely:

- Collectively, the covering nodes' `.tex` statements are at least as strong as *T*.
- They are not collectively stronger than *T* in a way the reference text does not justify.
- Variable conventions, quantifier scopes, definitional dependencies, and ambient hypotheses align between the covering set and the reference statement.

Read the reference text and all `.tex` definitions that affect the meaning of the covering statements.
