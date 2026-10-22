## Cargo builds (rbs)

`cargo` is an rbs shim that runs builds on a remote build server. From the
Bash sandbox this only works when the whole command is on the sandbox's
excluded list, so:

- Run cargo as its own command, or chained only with `cd`, `tail`, `head`
  or `grep` (e.g. `cd <worktree> && cargo test 2>&1 | tail -n 50`).
- Don't chain other commands with cargo (`touch`, `ls`, `sort`, loops,
  `ENV=value cargo ...`); run those as separate Bash calls.
- If output shows `skipping backend backend=remote` or `local server
  unavailable`, the build ran locally; rerun it in the plain form above.
