# Skill sources

Load on request when resyncing, reviewing provenance, or extending the smart-contract implementation
skill.

## Local vendored snapshot

- `../../vendor/ethskills/`
- `../../vendor/evm-audit-skills/`
- `../../vendor/pashov-skills/` — explicit named workflows only; see [pashov.md](pashov.md).
- Canonical vendor provenance and refresh doc: `../../vendor/sources.md`

Use local vendored files rather than remote runtime fetches. The Pashov pack is optional and
requires the explicit request and run approval described in its routing policy.

## Upstream sources used on April 24, 2026

- `https://ethskills.com/SKILL.md`
- `https://ethskills.com/ship/SKILL.md`
- `https://ethskills.com/concepts/SKILL.md`
- `https://ethskills.com/standards/SKILL.md`
- `https://ethskills.com/tools/SKILL.md`
- `https://ethskills.com/gas/SKILL.md`
- `https://ethskills.com/security/SKILL.md`
- `https://ethskills.com/testing/SKILL.md`
- `https://ethskills.com/audit/SKILL.md`
- `https://raw.githubusercontent.com/austintgriffith/evm-audit-skills/main/evm-audit-master/SKILL.md`

The wrapper skill is a synthesis layered on top of the vendored upstream snapshot. Refresh the
vendor directories first, then update the wrapper docs only if the workflow changed.

## Resync procedure

1. Refresh `../../vendor/ethskills/` from the upstream `ethskills` URLs above.
2. Refresh `../../vendor/evm-audit-skills/` from the upstream audit master and checklist URLs
   above.
3. Keep vendored skill-to-skill links local after each refresh.
4. Update [ethskills-routing.md](ethskills-routing.md) and [deep-audit.md](deep-audit.md) if the
   local vendor paths or routing assumptions changed.
5. Update [gas-optimization.md](gas-optimization.md) only if our local engineering guidance changes
   or new measured patterns clearly beat the current defaults.
6. Update [../SKILL.md](../SKILL.md) only when the actual workflow, stop conditions, or required
   deliverables change.
