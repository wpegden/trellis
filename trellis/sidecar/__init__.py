"""Parallel-closure sidecar daemon (SIDECAR plan §3.2).

An out-of-kernel background prover: reads the kernel-exported
``candidates.json``, iterates body-only proof attempts against a
Leanstral-class model in its OWN workspace, and publishes fully
prevalidated successes into the spool the kernel ingests at the
inter-cycle boundary. The kernel's apply gate is the sole authority;
everything here is scheduling + waste-minimisation.

Inert without the ``sidecar`` block in ``trellis.config.json``.
"""
