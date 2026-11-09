## What and why

<!-- One logical change. Link the issue or milestone it belongs to. -->

## Checklist

- [ ] I have signed the [CLA](../CLA.md) (or my employer signed the [Corporate CLA](../CLA-CORPORATE.md))
- [ ] The title follows Conventional Commits (`feat(agent): …`)
- [ ] `cargo xtask ci` passes locally
- [ ] Rust changes follow [docs/CONVENTIONS.md](../docs/CONVENTIONS.md)
- [ ] No invariant from [PLAN §3](../docs/PLAN.md#3-invariants-non-negotiable) is broken
- [ ] Docs and `CHANGELOG.md` (`Unreleased`) are updated if behaviour changed
- [ ] Parsers and tests use captured fixtures, or are marked `// UNVERIFIED`
