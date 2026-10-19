# Remote inputs (GATE-22)

A routed run sends a repo's declared ignored inputs to the builder's work folder before the run. All four steps are built; the design is in [design.md, Routing](../design.md#routing) (Inputs, and The remote client's run, Inputs) and [The box side](../design.md#the-box-side) (the `inputs` verb). The decisions I1 to I17 are there as current behaviour; this file keeps what the open issues GATE-332, GATE-334, GATE-335 and GATE-337 still name.

## Decisions the open issues name

**I2. Globs and exclusions** (GATE-334 changes the denylist). The built-in exclusion set is under "Never sent" in [design.md, Routing](../design.md#routing), Inputs.

**I10. A failed step runs the command here** (GATE-337 maps a builder `busy` or `layout` refusal and an unreachable builder to the reasons the other calls give them). The fallback reason `inputs` is described in [design.md, Routing](../design.md#routing), Inputs.

**I14. Destination safety** (GATE-335 adds the same refusal of a hard-linked file on the PC). The box side, including the refusal of a target with more than one link, is under `inputs` in [design.md, The box side](../design.md#the-box-side).

**I15. Pairs** (GATE-332). Each repo's own `inputs` list applies to its own tree; the manifest's paths are prefixed with the tree's folder name (`{leaf}/…`, `{sibling}/…`). Reached only once GATE-332 builds pairs; until then a pair whose repos declare inputs routes, is refused with reason `inputs` before any inputs call and runs here.
