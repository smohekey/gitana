# Migrating a repository to `require`

`require` is the strict trust policy: pushes to protected refs must carry a valid push certificate
(`gta push --signed`), and every newly introduced commit and annotated tag must be signed by a
trusted key. Moving an existing repository to it is safe and reversible.

## Existing history is grandfathered

Flipping to `require` does **not** retroactively reject existing history. Enforcement grandfathers
every object reachable from the current protected-ref tips (`refs/heads/*`, `refs/tags/*`,
`refs/gitana/*`) at the moment of each push; only objects a protected ref *newly* introduces must be
signed. You do not need to re-sign or rewrite history to enable `require`.

## Preview the cutover

Both `gta trust init` and `gta trust set-policy` accept `--dry-run`, which reports the impact and
writes nothing:

```sh
gta trust set-policy require --dry-run
```

It prints the current and target policy, the enrolled key fingerprints, and exactly what `require`
will demand of future pushes — so you can confirm the right keys are trusted before committing.

## Steps

1. **Enrol at least two signing keys.** A single-key `require` root locks the repository out if that
   key is lost, so `require` with fewer than two keys is refused unless you pass `--break-glass`:

   ```sh
   gta trust add-key <colleague.pub> --signing-key <your-key>
   ```

2. **Preview:** `gta trust set-policy require --dry-run`.

3. **Flip:** `gta trust set-policy require --signing-key <your-key>`.

4. **Sign your work from now on:** `gta commit -S`, `gta tag -s`, and `gta push --signed` — or set the
   git config conventions `user.signingkey` and `gpg.format=ssh` with `commit.gpgsign` / `tag.gpgSign`
   so signing happens automatically.

## Ease in with `warn` first

`require` is not a one-way door. Under `warn`, verification still runs and failures are recorded as
audit events, but pushes are **not** rejected — so you can observe what `require` would block before
enforcing it. Start at `warn`, watch the audit output, then `gta trust set-policy require` when the
signing habits are in place. To roll back, `gta trust set-policy warn` (or `off`), signed by a
still-trusted key.
