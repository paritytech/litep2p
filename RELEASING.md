# Release Checklist

These steps assume that you've checked out the Litep2p repository and are in the root directory of it.

We also assume that ongoing work done is being merged directly to the `master` branch.

Publishing to crates.io is automated by `.github/workflows/release-publish.yml`, which runs when a `v*` tag is pushed to the repo. The steps below cover preparing the release PR and triggering that workflow. (The prep steps are still manual today; once `release-prep.yml` lands they will be performed by a `workflow_dispatch` instead.)

## One-time setup

Before the first automated release can succeed, a crate owner must:

1. Create a `release` GitHub Environment under repo Settings → Environments with required reviewers configured. The publish job will not start until a reviewer approves it.
2. Generate a crates.io API token scoped to `publish-update` on the `litep2p` crate and add it as `CRATES_IO_TOKEN` to the `release` environment's secrets.

These do not need to be repeated for subsequent releases.

## Release steps

1. Ensure that everything you'd like to see released is on the `master` branch.

2. Create a release branch off `master`, for example `release-v0.15.0`. Decide how far the version needs to be bumped based on the changes to date. If unsure what to bump the version to (e.g. is it a major, minor or patch release), check with the Parity Networking team.

3. Bump the crate version in the root `Cargo.toml` to whatever was decided in step 2 (a find-and-replace from the old version to the new version in this file should do the trick).

4. Ensure the `Cargo.lock` file is up to date.

    ```bash
    cargo generate-lockfile
    ```

5. Update `CHANGELOG.md` to reflect the difference between this release and the last. If you're unsure of what to add, check with the Networking team. See the existing entries in `CHANGELOG.md` for the format.

    The publish workflow extracts the section for the new version verbatim and uses it as the GitHub Release body, so the heading must be exactly `## [X.Y.Z] - YYYY-MM-DD` (matching the existing entries). If you skip this step the GitHub Release notes will be empty.

    First, if there have been any significant changes, add a description of those changes to the top of the changelog entry for this release.

    Next, mention any merged PRs between releases.

6. Commit the above changes to the release branch and open a PR in GitHub with a base of `master`.

7. Once the branch has been reviewed and passes CI, merge it.

8. Pull `master`, then tag the merge commit and push the tag. **The version in the tag must match the version in `Cargo.toml` exactly** — the publish workflow refuses to run on a mismatch.

    ```bash
    git checkout master && git pull
    git tag -s v0.15.0   # use the version you just merged
    git push --tags
    ```

9. Go to the [Actions tab](https://github.com/paritytech/litep2p/actions/workflows/release-publish.yml) and watch the **Release Publish** workflow.

    - The `verify` job runs the full test suite and verifies the tag matches `Cargo.toml`. Review its result before approving the next step.
    - The `publish` job is gated by the `release` environment. Once `verify` passes, GitHub will request a reviewer's approval; once approved, it publishes to crates.io and creates a GitHub Release using the changelog section extracted in step 5.

## If something goes wrong

- **`verify` fails with "Tag version does not match Cargo.toml version"** — delete the bad tag (`git tag -d vX.Y.Z && git push --delete origin vX.Y.Z`), fix `Cargo.toml` on `master` (open another PR if needed), and re-tag.
- **`cargo publish` fails with "crate version X.Y.Z is already uploaded"** — the publish already happened in a previous run. Re-running the workflow will fail at this step, but the GitHub Release step is idempotent and will update the existing release. To recover, manually run the post-publish steps (extracting release notes and calling `gh release create/edit`) or push a new patch version.
- **The publish workflow never asks for approval** — confirm the `release` environment exists and has required reviewers set; without that the job will run immediately on tag push.
