# Contributing to LLM Gateway

Thanks for considering a contribution! This document covers how to submit
changes and the legal terms your contribution is made under.

## License & Contributor License Agreement

LLM Gateway is licensed under the **GNU AGPL-3.0** (see [`LICENSE`](LICENSE)).
croit GmbH additionally offers the software under separate commercial licenses.

To keep that dual-licensing model possible, every contribution must be covered
by our **Contributor License Agreement** ([`CLA.md`](CLA.md)). In short, the
CLA:

- lets **you keep ownership** of your contribution;
- grants croit GmbH a perpetual, worldwide, irrevocable, royalty-free,
  sublicensable license to your contribution — **including the right to
  relicense it under other terms, such as commercial/proprietary licenses**;
- includes a patent grant (Apache-2.0 §3 style);
- asks you to confirm you have the rights to contribute (and, if applicable,
  your employer's permission).

**How it's signed:** the first time you open a pull request, the
[CLA Assistant](https://github.com/cla-assistant/cla-assistant) bot will ask you
to accept the CLA with one click and records your acceptance against your GitHub
account. You only do this once.

## Developer Certificate of Origin (sign-off)

In addition to the CLA, we use the
[Developer Certificate of Origin](https://developercertificate.org/). Sign off
every commit:

```bash
git commit --signoff   # adds a "Signed-off-by: Your Name <you@example.com>" line
```

> Note: this repository's `commit-msg` hook rejects `Co-authored-by:` trailers.
> The `Signed-off-by` sign-off is required; co-author trailers are not allowed.

## Submitting changes

1. Fork the repo and create a topic branch.
2. Make your change. Match the surrounding code style.
3. Run the full local gate before pushing:
   ```bash
   mise run ci      # fmt + clippy (-D warnings) + tests
   ```
   (The `pre-push` hook runs the same checks.)
4. Open a pull request with a clear description of the change and motivation.
5. Accept the CLA when prompted, and make sure your commits are signed off.

## Questions

Open an issue, or reach us at <info@croit.io>.
