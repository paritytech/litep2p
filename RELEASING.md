# Release Checklist

Both stages of a litep2p release are automated:

- **Release Prep** (`.github/workflows/release-prep.yml`) — opens the release PR for you (bumps `Cargo.toml`, regenerates `Cargo.lock`, drafts a CHANGELOG entry, runs `cargo publish --dry-run` against the result).
- **Release Publish** (`.github/workflows/release-publish.yml`) — runs on `v*` tag push, executes the final test suite, publishes to crates.io, and creates the GitHub Release. The publish step is gated by the `release` environment and requires reviewer approval.

The steps below assume those workflows are healthy. A manual fallback is documented at the bottom.

## One-time setup

Before the first automated release can succeed, a crate owner must:

1. Create a `release` GitHub Environment under repo Settings → Environments with required reviewers configured. The publish job will not start until a reviewer approves it.
2. Generate a crates.io API token scoped to `publish-update` on the `litep2p` crate and add it as `CRATES_IO_TOKEN` to the `release` environment's secrets.

These do not need to be repeated for subsequent releases.

## Release steps

1. Ensure that everything you'd like to see released is on the `master` branch.

2. Go to the [Release Prep workflow](https://github.com/paritytech/litep2p/actions/workflows/release-prep.yml), click **Run workflow**, and supply the new `version` (e.g. `0.15.0`, no leading `v`). If unsure what to bump to, check with the Parity Networking team.

    The workflow will:
    - Bump `Cargo.toml` and regenerate `Cargo.lock`.
    - Prepend a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` with all squash-merge subjects since the previous tag, listed under a flat `### Changes` heading.
    - Run `cargo publish --dry-run` to verify the crate packages cleanly.
    - Push a `release-vX.Y.Z` branch and open a PR titled `Prepare release vX.Y.Z`.

3. Review the auto-opened PR:
    - Re-bucket the items under `### Changes` into `### Added` / `### Changed` / `### Fixed` to match the existing CHANGELOG style.
    - Add a summary paragraph at the top of the new section (or remove the TODO placeholder).
    - Confirm `Cargo.toml` and `Cargo.lock` look right.

    **If CI hasn't started on the PR**, push an empty commit to trigger it (GitHub does not run `pull_request` workflows on PRs opened by `GITHUB_TOKEN`):

    ```bash
    git fetch origin && git checkout release-v0.15.0
    git commit --allow-empty -m "trigger ci" && git push
    ```

4. Once the PR is reviewed and CI is green, merge it.

5. Pull `master`, then tag the merge commit and push the tag. **The version in the tag must match the version in `Cargo.toml` exactly** — the publish workflow refuses to run on a mismatch.

    ```bash
    git checkout master && git pull
    git tag -s v0.15.0   # use the version you just merged
    git push --tags
    ```

6. Go to the [Release Publish workflow](https://github.com/paritytech/litep2p/actions/workflows/release-publish.yml) and watch the run that was triggered by the tag push.

    - The `verify` job runs the full test suite and re-checks that the tag matches `Cargo.toml`. Review its result before approving the next step.
    - The `publish` job is gated by the `release` environment. Once `verify` passes, GitHub will request a reviewer's approval; once approved, it publishes to crates.io and creates a GitHub Release using the changelog section from step 3.

## If something goes wrong

- **Release Prep fails at `cargo publish --dry-run`** — the crate would fail to publish in its current state (e.g. a `Cargo.toml` field is missing, an `include`/`exclude` path is broken, or a path-dependency was left in). Fix the issue on `master` via a normal PR, then re-run Release Prep.
- **Release Prep finishes but I want to redo it** — re-running with the same version is supported: the workflow checks out the existing `release-vX.Y.Z` branch if it exists. If nothing changes, the commit step is skipped and the PR is left as-is. To force a fresh start instead, delete the branch (`git push --delete origin release-vX.Y.Z`) before re-running.
- **`verify` fails with "Tag version does not match Cargo.toml version"** — delete the bad tag (`git tag -d vX.Y.Z && git push --delete origin vX.Y.Z`), fix `Cargo.toml` on `master` (open another PR if needed), and re-tag.
- **`cargo publish` fails with "crate version X.Y.Z is already uploaded"** — the publish already happened in a previous run. The GitHub Release step is create-or-edit, so re-running the publish workflow after fixing the issue will still produce a well-formed release; you may instead need to bump to a new patch version.
- **The publish workflow never asks for approval** — confirm the `release` environment exists and has required reviewers set; without that the job will run immediately on tag push.

## Manual fallback

If either workflow is broken and a release is urgent, the prep can be done by hand:

1. Branch off `master` as `release-vX.Y.Z`.
2. Bump the `version` field in the root `Cargo.toml`.
3. Run `cargo generate-lockfile`.
4. Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (see existing entries for format — the heading must match exactly for the publish workflow's release-notes extractor).
5. Run `cargo publish --dry-run` locally to confirm the crate packages.
6. Open a PR to `master`, get it reviewed, and merge.
7. Continue from step 5 of the automated flow (tag the merge commit and push the tag).

Publishing itself should never be done manually — always go through the `release` environment gate to keep the audit trail.
