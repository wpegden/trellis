## Correspondence scratch workspace

A writable temporary correspondence scratch directory is available at:

`{{correspondence_scratch_path}}`

Use it for Isabelle probes or reduced examples that help compare the Isabelle
statement with the paired TeX statement. E.g., you can use this to test whether
you agree that the hypotheses of statement genuinely correspond to the precise
strength of the natural language version in the TeX file. (Since you only check
nodes that should also be substantive, any Isabelle statement that admits a
countermodel should fail correspondence.) Discovering a small countermodel is an
efficient way to discover subtle problems with Isabelle hypotheses early.

This directory is non-canonical and request-local; do not edit `Tablet/` or
other canonical repo files.

Example:

```bash
cd {{repo_path}}
isabelle build -d {{correspondence_scratch_path}} Probe
```
