-- Library root for the local_closure_smoke fixture.
-- Lake expects this file to exist alongside the `Tablet/` directory
-- when `lean_lib «Tablet»` is declared. It re-exports each fixture
-- node so `import Tablet` pulls them all in; individual tests use
-- `import Tablet.X` for one-node imports.

import Tablet.Preamble
import Tablet.Helper
import Tablet.Closed
import Tablet.UsesHelper
import Tablet.ActiveSorry
import Tablet.InductiveNat
import Tablet.UsesInductive
import Tablet.ReservedArtifactDef
import Tablet.UsesReservedArtifact
