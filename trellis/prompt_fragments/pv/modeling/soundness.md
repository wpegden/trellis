Judge the assigned NL arguments by a rigorous, line-by-line check from their stated hypotheses and explicitly cited support.

For a definition or Preamble, the arguments are its preservation sections: check each claim's proof and its separate application to every pursued GOAL. Use the paired NL definitions and relevant Rust source. Preamble covers the setup and adaptation changes; definition records cover their authored meanings. A claim must retain the distinctions and domains its goal applications use. Check the coverage of any representation or witness used to connect a GOAL’s domain to the model. Report missing source interpretations or unstated assumptions as gaps. Shared preservation reasoning may be applied briefly to each GOAL; deriving the GOAL conclusion is not required.

Inspect primary evidence cited with `\evidenceref{path}` and check that it establishes the preservation claim throughout its stated scope. Reports of earlier agent judgments do not supply that evidence.

For a theorem-like node, check its ordinary NL proof. A complete Lean proof can discharge an ordinary theorem through the existing checker; a preservation record may cite that theorem and justify its application.

Use `\noderef{NodeName}` for cross-node support. It cites the node's paired NL statement, not its preservation claims or proof. Lean interpretation belongs to correspondence; an insufficient NL description requires repair. Missing arguments and `SKETCH:` are incomplete.
