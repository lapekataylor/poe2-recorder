# Releasing

Flatpak is the only native release format. The stable application ID is
`io.github.lapekataylor.PoeRecorder`; the permanent signed remote is
`https://lapekataylor.github.io/poe2-recorder/`.

## Candidate build

1. Update `native/Cargo.toml` and the first `<release version="...">` in
   `data/io.github.lapekataylor.PoeRecorder.metainfo.xml` to the same version.
2. Run `bash scripts/generate-release-notes.sh <version>` as the last thing
   before the release commit. It prepends the commit subjects since the
   previous tag to `data/release-notes.md`, which is compiled into the binary
   and shown once, after the update, by the "What's new" dialog. The release
   commit itself is not in the list; keep it a version-bump-only commit.
3. Run the four standard Rust checks from the root.
4. Create an annotated tag whose name is exactly `v<version>` and push it. The
   tag workflow rejects a version whose Cargo, AppStream, or release-notes
   entries disagree before building.
5. Configure the `release` environment secrets:
   `FLATPAK_GPG_PRIVATE_KEY` contains the armored private key and
   `FLATPAK_GPG_KEY_ID` contains only its public key ID. Never commit the key
   or print either secret.
6. Download the workflow's release-candidate artifact and verify the recorded
   SHA-256. The bundle is suitable for disposable-user testing and does not
   configure a remote.

CI builds from the locked `native/Cargo.lock` and `flatpak/cargo-sources.json`,
uses the pinned GNOME 50 SDK/runtime, runs `flatpak-builder-lint` for manifest,
AppStream, and repository, and exports a signed static OSTree repository plus
one `.flatpak` bundle. The documented linter exceptions in
`flatpak/lint-exceptions.json` are intentional: the canonical reverse-DNS ID,
and the Wayland-only product scope. Rebuilding from the same commit should
produce the same application payload; OSTree/bundle container metadata may
vary by build time.
The AppStream screenshot is served from the committed `main` tree, while the
candidate repository also carries its mirrored `screenshots/x86_64` ref.

## Manual remote publication

Never run this for an unapproved candidate. After a candidate is approved,
publish the `repo/` directory produced by that build to the project GitHub
Pages site at the permanent URL. Keep
`index.flatpakrepo`, `summary`, the signed summary, objects, and static deltas
together. Verify with only the public key:

```sh
gpg --import public-release-key.asc
flatpak remote-add --user --if-not-exists poe-recorder \
  https://lapekataylor.github.io/poe2-recorder/index.flatpakrepo
flatpak install --user poe-recorder io.github.lapekataylor.PoeRecorder
flatpak remote-info --user --show-commit poe-recorder \
  io.github.lapekataylor.PoeRecorder
```

If the signing key is lost, users must remove and re-add the remote with a new
public key. There is no rotation framework or staging remote.

## Install, update, rollback, uninstall

For a stable install, add the permanent remote and install the application as
shown above. Update with `flatpak update --user`. For candidate testing use:

```sh
flatpak install --user ./poe-recorder.flatpak
```

To roll back to the previous signed deployment, inspect the remote log and
deploy its previous commit:

```sh
flatpak remote-info --user --log poe-recorder \
  io.github.lapekataylor.PoeRecorder
flatpak update --user --commit=<previous-commit> \
  io.github.lapekataylor.PoeRecorder
```

Uninstalling the app does not delete recordings:

```sh
flatpak uninstall --user io.github.lapekataylor.PoeRecorder
```

Use `--delete-data` only when deleting the native app's private data is
intentional.
