# Paper-Faithfulness

A paper target *T* — a specific claim in the paper that the formalization is responsible for proving — is **paper-faithful** in the tablet iff the *covering set* of *T* (the collection of tablet nodes whose `.tex` statements collectively represent *T*) faithfully expresses the full mathematical content of *T* as the paper states it.

Concretely, the covering set is paper-faithful when:

- Collectively, the covering nodes' `.tex` statements are at least as strong as *T*.
- They are not collectively stronger than *T* in a way the paper does not justify.
- Variable conventions, quantifier scopes, definitional dependencies, and ambient hypotheses align between the covering set and the paper's statement of *T*.

Note that checking paper-faithfulness thus requires reading all the `.tex` definitions in the tablet that affect the meaning of the `.tex` of the covering nodes.
